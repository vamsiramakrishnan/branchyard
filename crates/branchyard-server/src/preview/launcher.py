"""Apply process limits before loading the trusted document-preview helper."""

import os
import resource
import runpy
import sys

resource.setrlimit(resource.RLIMIT_CPU, (35, 35))
resource.setrlimit(resource.RLIMIT_FSIZE, (32 * 1024 * 1024, 32 * 1024 * 1024))
resource.setrlimit(resource.RLIMIT_AS, (2 * 1024 * 1024 * 1024, 2 * 1024 * 1024 * 1024))
resource.setrlimit(resource.RLIMIT_NOFILE, (128, 128))
os.umask(0o077)
sys.path.insert(0, "/helper")
sys.argv = ["branchyard_artifacts", "preview", *sys.argv[1:]]
runpy.run_module("branchyard_artifacts", run_name="__main__", alter_sys=True)
