#!/bin/sh
command -v by >/dev/null 2>&1 && by hook --harness fixture "$1" || true
