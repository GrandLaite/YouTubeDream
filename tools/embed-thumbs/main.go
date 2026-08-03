// embed-thumbs — одноразовая утилита: проходит папку рекурсивно, для каждого
// найденного mp4/m4a/mp3 ищет соответствующий .jpg/.jpeg/.png/.webp в _thumbs/
// или рядом и встраивает его в файл как обложку через ffmpeg. После успешного
// embed превью-файл удаляется. Пустые _thumbs/ убираются в конце.
//
// Использование:
//
//	embed-thumbs.exe                       # текущая директория
//	embed-thumbs.exe "D:\YT\МойКанал"      # конкретная папка
//	embed-thumbs.exe -dry-run "D:\YT"      # без модификации файлов
package main

import (
	"bytes"
	"flag"
	"fmt"
	"io/fs"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
)

func main() {
	dry := flag.Bool("dry-run", false, "только показать что будет сделано")
	flag.Parse()

	root := "."
	if flag.NArg() > 0 {
		root = flag.Arg(0)
	}
	abs, err := filepath.Abs(root)
	if err == nil {
		root = abs
	}

	if _, err := exec.LookPath("ffmpeg"); err != nil {
		fmt.Fprintln(os.Stderr, "ffmpeg не найден в PATH")
		os.Exit(1)
	}

	fmt.Printf("Папка: %s\n", root)
	if *dry {
		fmt.Println("Режим: dry-run (файлы не изменяются)")
	}
	fmt.Println()

	var processed, embedded, skipped, failed int

	err = filepath.WalkDir(root, func(path string, d fs.DirEntry, err error) error {
		if err != nil {
			return nil
		}
		if d.IsDir() {
			if d.Name() == "_thumbs" {
				return filepath.SkipDir
			}
			return nil
		}
		ext := strings.ToLower(filepath.Ext(path))
		if ext != ".mp4" && ext != ".m4a" && ext != ".mp3" {
			return nil
		}
		processed++

		thumb := findThumbnail(path)
		if thumb == "" {
			skipped++
			return nil
		}

		rel, _ := filepath.Rel(root, path)
		fmt.Printf("[%d] %s\n        ← %s\n", processed, rel, filepath.Base(thumb))

		if *dry {
			return nil
		}

		if err := embedThumbnail(path, thumb); err != nil {
			failed++
			fmt.Fprintf(os.Stderr, "        ошибка: %v\n", err)
			return nil
		}
		embedded++
		if err := os.Remove(thumb); err != nil {
			fmt.Fprintf(os.Stderr, "        не удалось удалить превью: %v\n", err)
		}
		return nil
	})
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}

	if !*dry {
		cleanEmptyThumbDirs(root)
	}

	fmt.Printf("\nГотово. Найдено: %d, embed: %d, без превью: %d, ошибок: %d\n",
		processed, embedded, skipped, failed)
}

// findThumbnail ищет превью для файла: сначала в _thumbs/<base>.<ext>,
// затем рядом с самим файлом.
func findThumbnail(mediaPath string) string {
	dir := filepath.Dir(mediaPath)
	base := strings.TrimSuffix(filepath.Base(mediaPath), filepath.Ext(mediaPath))
	for _, ext := range []string{".jpg", ".jpeg", ".png", ".webp"} {
		for _, c := range []string{
			filepath.Join(dir, "_thumbs", base+ext),
			filepath.Join(dir, base+ext),
		} {
			if _, err := os.Stat(c); err == nil {
				return c
			}
		}
	}
	return ""
}

// embedThumbnail встраивает обложку через ffmpeg без перекодирования (-c copy).
// Запись во временный файл + Rename — при ошибке исходник остаётся целым.
func embedThumbnail(mediaPath, thumbPath string) error {
	ext := strings.ToLower(filepath.Ext(mediaPath))
	tmp := mediaPath + ".embed_tmp" + ext

	var args []string
	switch ext {
	case ".mp4", ".m4a", ".mov":
		args = []string{
			"-loglevel", "error",
			"-i", mediaPath,
			"-i", thumbPath,
			"-map", "0",
			"-map", "1",
			"-c", "copy",
			"-disposition:v:1", "attached_pic",
			"-y", tmp,
		}
	case ".mp3":
		args = []string{
			"-loglevel", "error",
			"-i", mediaPath,
			"-i", thumbPath,
			"-map", "0:0",
			"-map", "1:0",
			"-c", "copy",
			"-id3v2_version", "3",
			"-metadata:s:v", "title=Album cover",
			"-metadata:s:v", "comment=Cover (front)",
			"-y", tmp,
		}
	default:
		return fmt.Errorf("неподдерживаемый формат: %s", ext)
	}

	cmd := exec.Command("ffmpeg", args...)
	var stderr bytes.Buffer
	cmd.Stderr = &stderr
	if err := cmd.Run(); err != nil {
		_ = os.Remove(tmp)
		msg := strings.TrimSpace(stderr.String())
		if msg == "" {
			msg = err.Error()
		}
		return fmt.Errorf("ffmpeg: %s", msg)
	}
	return os.Rename(tmp, mediaPath)
}

// cleanEmptyThumbDirs удаляет пустые папки _thumbs/ после успешного embed.
func cleanEmptyThumbDirs(root string) {
	_ = filepath.WalkDir(root, func(path string, d fs.DirEntry, err error) error {
		if err != nil || !d.IsDir() || d.Name() != "_thumbs" {
			return nil
		}
		entries, err := os.ReadDir(path)
		if err == nil && len(entries) == 0 {
			_ = os.Remove(path)
		}
		return nil
	})
}
