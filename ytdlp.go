package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"sort"
	"strconv"
	"strings"
)

// VideoInfo — информация о видео, возвращаемая фронтенду.
type VideoInfo struct {
	Title     string    `json:"title"`
	Thumbnail string    `json:"thumbnail"`
	Duration  int       `json:"duration"` // секунды
	Uploader  string    `json:"uploader"`
	Qualities []Quality `json:"qualities"`
}

// Quality — доступное качество видео.
type Quality struct {
	Height int    `json:"height"`
	Label  string `json:"label"`
	FPS    int    `json:"fps"`
}

// DownloadOptions — параметры загрузки от фронтенда.
type DownloadOptions struct {
	URL     string `json:"url"`
	Quality string `json:"quality"` // "audio" | "360" | "720" | "1080" | ...
	Format  string `json:"format"`  // "mp4" | "webm" | "mkv" | "mp3" | "m4a"
	OutDir  string `json:"outDir"`
}

// Progress — событие прогресса загрузки.
type Progress struct {
	Type     string  `json:"type"`    // "progress" | "merge" | "done" | "error"
	Percent  float64 `json:"percent"` // 0–100
	Speed    string  `json:"speed"`
	ETA      string  `json:"eta"`
	Size     string  `json:"size"`
	Message  string  `json:"message"`
	FilePath string  `json:"filePath"`
}

// ytDLPRaw — структура для парсинга JSON от yt-dlp -J.
type ytDLPRaw struct {
	Title     string                   `json:"title"`
	Thumbnail string                   `json:"thumbnail"`
	Duration  float64                  `json:"duration"`
	Uploader  string                   `json:"uploader"`
	Channel   string                   `json:"channel"`
	Formats   []map[string]interface{} `json:"formats"`
}

var (
	progressRe = regexp.MustCompile(
		`\[download\]\s+([\d.]+)%\s+of\s+~?([\d.]+\s*\w+)\s+at\s+([\d.]+\s*\w+/s)\s+ETA\s+(\S+)`,
	)
	destRe  = regexp.MustCompile(`\[download\] Destination: (.+)`)
	mergeRe = regexp.MustCompile(`\[Merger\]|Merging formats into`)
)

// CheckDeps проверяет наличие yt-dlp и ffmpeg.
func CheckDeps() (ytdlpOK bool, ffmpegOK bool) {
	_, err := exec.LookPath("yt-dlp")
	ytdlpOK = err == nil
	_, err = exec.LookPath("ffmpeg")
	ffmpegOK = err == nil
	return
}

// GetVideoInfo получает информацию о видео через yt-dlp -J.
func GetVideoInfo(ctx context.Context, url string) (*VideoInfo, error) {
	cmd := exec.CommandContext(ctx, "yt-dlp",
		"--no-playlist",
		"--no-warnings",
		"-J",
		url,
	)

	var stdout, stderr bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr

	if err := cmd.Run(); err != nil {
		msg := strings.TrimSpace(stderr.String())
		if msg == "" {
			msg = err.Error()
		}
		return nil, fmt.Errorf("%s", msg)
	}

	var raw ytDLPRaw
	if err := json.Unmarshal(stdout.Bytes(), &raw); err != nil {
		return nil, fmt.Errorf("failed to parse yt-dlp output: %w", err)
	}

	uploader := raw.Uploader
	if uploader == "" {
		uploader = raw.Channel
	}

	// Собираем уникальные высоты из форматов с видеопотоком.
	type heightFPS struct{ fps int }
	heights := map[int]heightFPS{}

	for _, f := range raw.Formats {
		vcodec, _ := f["vcodec"].(string)
		if vcodec == "" || vcodec == "none" {
			continue
		}
		h, ok := f["height"].(float64)
		if !ok || h <= 0 {
			continue
		}
		hi := int(h)
		fps := 0
		if v, ok := f["fps"].(float64); ok {
			fps = int(v)
		}
		if existing, exists := heights[hi]; !exists || fps > existing.fps {
			heights[hi] = heightFPS{fps: fps}
		}
	}

	sortedH := make([]int, 0, len(heights))
	for h := range heights {
		sortedH = append(sortedH, h)
	}
	sort.Slice(sortedH, func(i, j int) bool { return sortedH[i] > sortedH[j] })

	qualities := make([]Quality, 0, len(sortedH)+1)
	for _, h := range sortedH {
		label := fmt.Sprintf("%dp", h)
		switch {
		case h >= 2160:
			label = fmt.Sprintf("4K (%dp)", h)
		case h >= 1440:
			label = fmt.Sprintf("2K (%dp)", h)
		}
		if fps := heights[h].fps; fps > 30 {
			label += fmt.Sprintf(" %dfps", fps)
		}
		qualities = append(qualities, Quality{Height: h, Label: label, FPS: heights[h].fps})
	}

	return &VideoInfo{
		Title:     raw.Title,
		Thumbnail: raw.Thumbnail,
		Duration:  int(raw.Duration),
		Uploader:  uploader,
		Qualities: qualities,
	}, nil
}

// buildArgs формирует аргументы yt-dlp для заданного качества и формата.
//
// Стратегия по контейнерам/кодекам (обходим проблемы с OPUS):
//
//  mp4:
//    1. Предпочитаем H.264 видео + AAC аудио (m4a) → прямое мультиплексирование в mp4, без перекодирования.
//    2. Фолбек: любое видео + AAC → ffmpeg смержит, перекодируя видео если нужно.
//    3. Крайний фолбек: любое видео + любое аудио → ffmpeg перекодирует OPUS→AAC автоматически.
//    --merge-output-format mp4 заставляет yt-dlp/ffmpeg всегда отдавать mp4.
//
//  webm:
//    Берём VP9/AV1 + OPUS — оба нативны для webm, слияние без потерь.
//
//  mkv:
//    Лучшее качество без перекодирования: любые кодеки мультиплексируются в MKV.
//
//  mp3 / m4a:
//    Только аудио, --extract-audio конвертирует.
func buildArgs(opts DownloadOptions, outputTemplate string) []string {
	args := []string{
		"--no-playlist",
		"--newline",
		"-o", outputTemplate,
	}

	h := opts.Quality

	switch {
	case opts.Quality == "audio":
		switch opts.Format {
		case "mp3":
			args = append(args,
				"-f", "bestaudio[ext=m4a]/bestaudio",
				"--extract-audio",
				"--audio-format", "mp3",
				"--audio-quality", "0",
			)
		case "m4a":
			args = append(args,
				"-f", "bestaudio[ext=m4a]/bestaudio",
				"--extract-audio",
				"--audio-format", "m4a",
			)
		default: // opus
			args = append(args,
				"-f", "bestaudio[ext=webm]/bestaudio",
				"--extract-audio",
				"--audio-format", "opus",
			)
		}

	case opts.Format == "mp4":
		// Уровни предпочтения: H264+AAC > anyVideo+AAC > anyVideo+anyAudio
		fmtStr := fmt.Sprintf(
			"bestvideo[height<=%s][vcodec^=avc1]+bestaudio[ext=m4a]"+
				"/bestvideo[height<=%s][ext=mp4]+bestaudio[ext=m4a]"+
				"/bestvideo[height<=%s]+bestaudio[ext=m4a]"+
				"/bestvideo[height<=%s]+bestaudio"+
				"/best[height<=%s]",
			h, h, h, h, h,
		)
		args = append(args,
			"-f", fmtStr,
			"--merge-output-format", "mp4",
		)

	case opts.Format == "webm":
		// VP9/AV1 + OPUS → webm без перекодирования.
		fmtStr := fmt.Sprintf(
			"bestvideo[height<=%s][ext=webm]+bestaudio[ext=webm]"+
				"/bestvideo[height<=%s]+bestaudio",
			h, h,
		)
		args = append(args,
			"-f", fmtStr,
			"--merge-output-format", "webm",
		)

	default: // mkv — лучшее качество, любые кодеки.
		fmtStr := fmt.Sprintf(
			"bestvideo[height<=%s]+bestaudio/best[height<=%s]",
			h, h,
		)
		args = append(args,
			"-f", fmtStr,
			"--merge-output-format", "mkv",
		)
	}

	return args
}

// StartDownload запускает загрузку и отправляет события прогресса в канал.
// Канал закрывается по завершении.
func StartDownload(ctx context.Context, opts DownloadOptions, progressCh chan<- Progress) {
	defer close(progressCh)

	outDir := opts.OutDir
	if outDir == "" {
		home, _ := os.UserHomeDir()
		outDir = filepath.Join(home, "Downloads")
	}

	outputTemplate := filepath.Join(outDir, "%(title)s.%(ext)s")
	args := buildArgs(opts, outputTemplate)
	args = append(args, opts.URL)

	cmd := exec.CommandContext(ctx, "yt-dlp", args...)

	stdoutPipe, err := cmd.StdoutPipe()
	if err != nil {
		progressCh <- Progress{Type: "error", Message: err.Error()}
		return
	}

	var stderrBuf bytes.Buffer
	cmd.Stderr = &stderrBuf

	if err := cmd.Start(); err != nil {
		progressCh <- Progress{Type: "error", Message: err.Error()}
		return
	}

	var lastFile string

	scanner := bufio.NewScanner(stdoutPipe)
	for scanner.Scan() {
		line := scanner.Text()

		if m := progressRe.FindStringSubmatch(line); len(m) == 5 {
			pct, _ := strconv.ParseFloat(m[1], 64)
			progressCh <- Progress{
				Type:    "progress",
				Percent: pct,
				Size:    m[2],
				Speed:   m[3],
				ETA:     m[4],
			}
			continue
		}

		if m := destRe.FindStringSubmatch(line); len(m) == 2 {
			lastFile = strings.TrimSpace(m[1])
			continue
		}

		if mergeRe.MatchString(line) {
			progressCh <- Progress{
				Type:    "merge",
				Percent: 99,
				Message: "Объединение видео и аудио…",
			}
			continue
		}

		// Финальная строка "[download] 100%"
		if strings.Contains(line, "[download] 100%") {
			progressCh <- Progress{Type: "progress", Percent: 100}
		}
	}

	if err := cmd.Wait(); err != nil {
		errMsg := strings.TrimSpace(stderrBuf.String())
		if errMsg == "" {
			errMsg = err.Error()
		}
		// Игнорируем отмену контекста как "ошибку".
		if ctx.Err() != nil {
			return
		}
		progressCh <- Progress{Type: "error", Message: errMsg}
		return
	}

	// Если файл сохранён под другим именем (после post-processing), попробуем найти.
	finalFile := lastFile
	if finalFile != "" {
		// yt-dlp может поменять расширение (напр. .webm → .mp4 после merge).
		base := strings.TrimSuffix(finalFile, filepath.Ext(finalFile))
		for _, ext := range []string{".mp4", ".webm", ".mkv", ".mp3", ".m4a", ".opus"} {
			candidate := base + ext
			if _, err := os.Stat(candidate); err == nil {
				finalFile = candidate
				break
			}
		}
	}

	progressCh <- Progress{
		Type:     "done",
		Percent:  100,
		Message:  "Загрузка завершена!",
		FilePath: finalFile,
	}
}
