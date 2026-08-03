//go:build windows

package main

import (
	"os/exec"
	"strconv"
	"syscall"
)

// hideConsole прячет окно консоли при запуске дочернего процесса на Windows.
func hideConsole(cmd *exec.Cmd) {
	if cmd.SysProcAttr == nil {
		cmd.SysProcAttr = &syscall.SysProcAttr{}
	}
	cmd.SysProcAttr.HideWindow = true
	cmd.SysProcAttr.CreationFlags |= 0x08000000 // CREATE_NO_WINDOW
}

// killProcessTree убивает процесс и всё его дерево потомков. На Windows
// cmd.Process.Kill() убивает только сам процесс, а дочерние ffmpeg продолжают
// работать — поэтому используем taskkill /T (tree) /F (force).
func killProcessTree(pid int) {
	if pid <= 0 {
		return
	}
	cmd := exec.Command("taskkill", "/F", "/T", "/PID", strconv.Itoa(pid))
	hideConsole(cmd)
	_ = cmd.Run()
}
