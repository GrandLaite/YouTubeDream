//go:build !windows

package main

import (
	"os"
	"os/exec"
)

func hideConsole(_ *exec.Cmd) {}

func killProcessTree(pid int) {
	if pid <= 0 {
		return
	}
	if proc, err := os.FindProcess(pid); err == nil {
		_ = proc.Kill()
	}
}
