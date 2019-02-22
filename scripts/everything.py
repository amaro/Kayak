#!/usr/bin/python
#
# Copyright (c) 2018 University of Utah
#
# Permission to use, copy, modify, and distribute this software for any
# purpose with or without fee is hereby granted, provided that the above
# copyright notice and this permission notice appear in all copies.
#
# THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR(S) DISCLAIM ALL WARRANTIES
# WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
# MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL AUTHORS BE LIABLE FOR
# ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
# WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
# ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
# OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

import argparse
import subprocess
import pprint
import logging
import sys
import xml.etree.ElementTree as ET
import multiprocessing.pool as pool


class SubprocessException(Exception):
    def __init__(self, host, cmd, returnCode):
        self.host = host
        self.cmd = cmd
        self.returnCode = returnCode


def logProcess(logger, cmd, pout, perr):
    logger.debug(cmd)

    poutString = pout.decode("UTF-8")
    if poutString != '':
        logger.debug(poutString)

    perrString = perr.decode("UTF-8")
    if perrString != '':
        logger.error(perrString)


class Host(object):
    def __init__(self, name, id, hostName):
        self.__name = name
        self.__id = id
        self.__hostName = hostName

    def getPublicName(self):
        return self.__name + '.utah.cloudlab.us'

    def dump(self, level):
        logger.log(level, '\t\tName: ' + self.__name)
        logger.log(level, '\t\tId: ' + self.__id)
        logger.log(level, '\t\thostName: ' + self.__hostName)

    def ssh(self, logger, user, cmd):
        cmd = 'ssh {0}@{1} {2}'.format(user, self.getPublicName(), cmd)
        process = subprocess.Popen(cmd,
                                   shell=True,
                                   stdin=subprocess.PIPE,
                                   stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE,
                                   close_fds=True)
        pout, perr = process.communicate()
        logProcess(logger, cmd, pout, perr)

        if process.returncode:
            raise SubprocessException(self, cmd, process.returncode)


class Cluster(object):
    def __init__(self, logger, user, server):
        self.__logger = logger
        self.__server = None
        self.__clients = []
        self.__user = user

        cmd = 'ssh {0}@{1} /usr/bin/geni-get manifest'.format(
            self.__user, server)
        process = subprocess.Popen(cmd,
                                   shell=True,
                                   stdin=subprocess.PIPE,
                                   stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE,
                                   close_fds=True)
        pout, perr = process.communicate()
        logProcess(self.__logger, cmd, pout, perr)

        root = ET.fromstring(pout.decode("UTF-8"))
        for xmlNode in list(root):
            if not xmlNode.tag.endswith('node'):
                continue

            for child in list(xmlNode):
                if child.tag.endswith('host'):
                    hostName = child.get('name')

                if child.tag.endswith('vnode'):
                    name = child.get('name')

            id = xmlNode.get('client_id')
            if id == 'server' and self.__server == None:
                self.__server = Host(name, id, hostName)
            else:
                self.__clients.append(Host(name, id, hostName))
        if server == None:
            raise Exception("No server found!")

    def dump(self, level):
        self.__logger.log(level, '1 server:')
        self.__server.dump(level)

        self.__logger.log(level, '{} clients:'.format(len(self.__clients)))
        for i, client in enumerate(self.__clients):
            self.__logger.log(level, '\t{}:'.format(i))
            client.dump(level)

    def __executeOnClients(self, cmd):
        tpool = pool.ThreadPool(processes=len(self.__clients))
        async_results = []
        for client in self.__clients:

            def wrapSSHFunc(logger, user, cmd):
                try:
                    client.ssh(logger, user, cmd)
                except SubprocessException as e:
                    return e

            async_result = tpool.apply_async(
                wrapSSHFunc, (self.__logger, self.__user, cmd))
            async_results.append(async_result)

        for async_result in async_results:
            return_val = async_result.get()
            if return_val == None:
                continue
            elif isinstance(return_val, SubprocessException):
                raise return_val
            elif isinstance(return_val, Exception):
                raise return_val
            else:
                raise Exception(
                    'Unrecognized return value from ssh: {0}'.format(str(return_val)))

    def __executeOnServer(self, cmd):
        self.__server.ssh(self.__logger, self.__user, cmd)

    def setup(self, branch):
        try:
            logger.info("Server setup started...")
            self.__executeOnServer('"git clone https://github.com/utah-scs/splinter.git"')
            self.__executeOnServer('"cd splinter; git checkout {0}"'.format(branch))
            self.__executeOnServer('"cd splinter; ./scripts/setup.py --full"')  # beware, dirty trick. nic_info is local
            self.__executeOnServer('"cd splinter; cat nic_info" > nic_info')

            pci = subprocess.check_output("awk '/^pci/ { print $2; }' < nic_info", shell=True)
            if not pci:
                raise Exception("Failed to gather pci!")

            mac = subprocess.check_output("awk '/^mac/ { print $2; }' < nic_info", shell=True)
            if not pci or not mac:
                raise Exception("Failed to gather mac!")

            self.__executeOnServer("cd splinter; .cp db/server.toml-example db/server.toml; \
                                 sed -E -i 's/[0-9a-fA-F:]{17}/" + mac + "/' db/server.toml; \
                                 sed -E -i 's/0000:04:00.1/" + pci + "/' db/server.toml")
            self.__logger.info('Server setup concluded.')

        except Exception as e:
            self.__logger.error('Server setup failed!')
            self.__logger.error(str(e))
            exit(1)

        try:
            self.__logger.info("Clients setup started...")
            self.__executeOnClients('"git clone https://github.com/utah-scs/splinter.git"')
            self.__executeOnClients('"cd splinter; git checkout {0}"'.format(branch))
            self.__executeOnClients('"cd splinter; ./scripts/setup.py --full"')

            self.__executeOnClients('cd splinter; \
                                 echo "server_mac: ' + mac + '" >> nic_info; \
                                 ./scripts/create-client-toml')

            self.__logger.info('Clients setup concluded.')

        except Exception as e:
            self.__logger.error('Clients setup failed!')
            self.__logger.error(str(e))
            exit(1)

    def wipe(self,):
        try:
            self.__logger.info("Server wipe started...")
            self.__executeOnServer('"rm -rf splinter"')
            self.__logger.info("Server wipe concluded...")

        except Exception as e:
            self.__logger.error('Server wipe failed!')
            self.__logger.error(str(e))
            exit(1)

        try:
            self.__logger.info("Clients wipe started...")
            self.__executeOnClients('"rm -rf splinter"')
            self.__logger.info("Clients wipe concluded...")

        except Exception as e:
            self.__logger.error('Server wipe failed!')
            self.__logger.error(str(e))
            exit(1)

    def build(self, branch):
        try:
            self.__logger.info("Server build started...")
            self.__executeOnServer('"cd splinter; git checkout {0}"'.format(branch))
            self.__executeOnServer('"cd splinter; git pull"')
            self.__executeOnServer('"cd splinter; source ~/.cargo/env; make;"')
            self.__logger.info("Server build concluded...")

        except Exception as e:
            self.__logger.error('Server build failed!')
            self.__logger.error(str(e))
            exit(1)

        try:
            self.__logger.info("Clients build started...")
            self.__executeOnClients('"cd splinter; git checkout {0}"'.format(branch))
            self.__executeOnClients('"cd splinter; git pull"')
            self.__executeOnClients('"cd splinter; source ~/.cargo/env; make;"')
            self.__logger.info("Clients build concluded...")

        except Exception as e:
            self.__logger.error('Clients build failed!')
            self.__logger.error(str(e))
            exit(1)

    def startServer(self):
        try:
            self.__logger.info("Server start started...")
            self.__executeOnServer('"cd splinter; sudo scripts/run-server"')
            self.__logger.info("Server start concluded...")

        except Exception as e:
            self.__logger.error('Server start failed!')
            self.__logger.error(str(e))
            exit(1)

    def killServer(self):
        # TODO @jmbarzee implement kill server
        raise NotImplementedError()

    def startClients(self, ext):
        # TODO @jmbarzee add flexibility to extensions
        try:
            self.__logger.info("Clients kill started...")
            self.__executeOnServer('"cd splinter; sudo ./scripts/run-' + ext + ' 250000"')
            self.__logger.info("Clients kill concluded...")

        except Exception as e:
            self.__logger.error('Clients kill failed!')
            self.__logger.error(str(e))
            exit(1)
        
    def killClients(self, ext):
        try:
            self.__logger.info("Clients kill started...")
            self.__executeOnServer('"sudo kill -9 `pidof ' + ext + '`"')
            self.__logger.info("Clients kill concluded...")

        except Exception as e:
            self.__logger.error('Clients kill failed!')
            self.__logger.error(str(e))
            exit(1)

    def bench(self):
        # TODO @jmbarzee implement bench
        # TODO @jmbarzee configure logging correctly
        # TODO @jmbarzee symlink latest (.log, .extract)
        raise NotImplementedError()


if __name__ == '__main__':
    parser = argparse.ArgumentParser(
        description='Setup a machine for Sandstorm')

    parser.add_argument('-v',
                        help='Logging level. 10 for debug',
                        nargs='?',
                        type=int,
                        default=30,
                        const=20,
                        choices=range(0, 51),
                        metavar='lvl')

    parser.add_argument('-b',
                        help='Specifies branch to be used.',
                        nargs='?',
                        default='master',
                        const='current',
                        metavar='brch')

    parser.add_argument('-e',
                        help='Specifies extension to be used.',
                        nargs='?',
                        default='ycsb',
                        metavar='ext')

    parser.add_argument('--setup',
                        help='setup the cluster (clone, setup.py, etc.)',
                        action='store_false',
                        default=False)

    parser.add_argument('--wipe',
                        help='wipe the repository before building (rm splinter)',
                        action='store_false',
                        default=False)

    parser.add_argument('--build',
                        help='build splinter on the cluster (push local, pull, make)',
                        action='store_false',
                        default=False)

    parser.add_argument('user',
                        help='the user for ssh',
                        metavar='user')

    parser.add_argument('server',
                        help='the server of the cluster. e.g. "hp174.utah.cloudlab.us"',
                        metavar='server')

    parser.add_argument('command',
                        help='instructions for the script [run|kill|bench]',
                        choices=['run', 'kill', 'bench'],
                        metavar='command')

    args = parser.parse_args()


    # TODO @jmbarzee change logging location.
    logging.basicConfig(filename='test.log', level=logging.DEBUG)
    logger = logging.getLogger('')
    logger.setLevel(args.v)

    try:
        cluster = Cluster(logger, args.user, args.server)
    except Exception as e:
        logger.error("Could not establish cluster information!")
        cluster.dump(logging.ERROR)
        exit(1)

    cluster.dump(logging.INFO)

    if args.setup:
        cluster.setup(args.branch)

    if args.wipe:
        cluster.wipe()

    if args.build:
        cluster.build(args.branch)

    cmd = args.command
    if cmd == "run":
        # TODO @jmbarzee check for -e
        cluster.startServer()
        cluster.startClients(args.e)
        cluster.killServer()

    elif cmd == "kill":
        # TODO @jmbarzee check for -e
        cluster.killClients(args.e)
        cluster.killServer()


