# -*- coding: utf-8 -*-
from __future__ import (absolute_import, division, print_function,
                        unicode_literals)
import os

from stgit.compat import text
from stgit.utils import strip_suffix

__copyright__ = """
Copyright (C) 2005, Catalin Marinas <catalin.marinas@gmail.com>
Copyright (C) 2008, Karl Hasselström <kha@treskal.com>

This program is free software; you can redistribute it and/or modify
it under the terms of the GNU General Public License version 2 as
published by the Free Software Foundation.

This program is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
GNU General Public License for more details.

You should have received a copy of the GNU General Public License
along with this program; if not, see http://www.gnu.org/licenses/.
"""


def get_command(mod):
    """Import and return the given command module."""
    return __import__(__name__ + '.' + mod, globals(), locals(), ['*'])

_kinds = [('repo', 'Repository commands'),
          ('stack', 'Stack (branch) commands'),
          ('patch', 'Patch commands'),
          ('wc', 'Index/worktree commands'),
          ('alias', 'Alias commands')]
_kind_order = [kind for kind, desc in _kinds]
_kinds = dict(_kinds)

def _find_commands():
    for p in __path__:
        for fn in os.listdir(p):
            if not fn.endswith('.py'):
                continue
            mod = text(strip_suffix('.py', fn))
            m = get_command(mod)
            if not hasattr(m, 'usage'):
                continue
            yield mod, m

def get_commands(allow_cached = True):
    """Return a map from command name to a tuple of module name, command
    type, and one-line command help."""
    if allow_cached:
        try:
            from stgit.commands.cmdlist import command_list
            return command_list
        except ImportError:
            # cmdlist.py doesn't exist, so do it the expensive way.
            pass
    return dict((text(getattr(m, 'name', mod)), (mod, _kinds[m.kind], m.help))
                for mod, m in _find_commands())

def py_commands(commands, f):
    f.write('from __future__ import unicode_literals\n\n')
    f.write('command_list = {\n')
    for name, (mod, kind, help) in commands.items():
        f.write('    %r: (\n' % name)
        f.write('        %r,\n' % mod)
        f.write('        %r,\n' % kind)
        f.write('        %r,\n' % help)
        f.write('    ),\n')
    f.write('}\n')

def _command_list(commands):
    kinds = {}
    for cmd, (mod, kind, help) in commands.items():
        kinds.setdefault(kind, {})[cmd] = help
    for kind in _kind_order:
        kind = _kinds[kind]
        try:
            yield kind, sorted(kinds[kind].items())
        except KeyError:
            pass

def pretty_command_list(commands, f):
    cmd_len = max(len(cmd) for cmd in commands)
    sep = ''
    for kind, cmds in _command_list(commands):
        f.write(sep)
        sep = '\n'
        f.write('%s:\n' % kind)
        for cmd, help in cmds:
            f.write('  %*s  %s\n' % (-cmd_len, cmd, help))

def _write_underlined(s, u, f):
    f.write(s + '\n')
    f.write(u*len(s) + '\n')

def asciidoc_command_list(commands, f):
    for kind, cmds in _command_list(commands):
        _write_underlined(kind, '~', f)
        f.write('\n')
        for cmd, help in cmds:
            f.write('linkstg:%s[]::\n' % cmd)
            f.write('    %s\n' % help)
        f.write('\n')
