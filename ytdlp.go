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
	URL            string `json:"url"`
	Quality        string `json:"quality"` // "audio" | "360" | "720" | "1080" | ...
	Format         string `json:"format"`  // "mp4" | "webm" | "mkv" | "mp3" | "m4a"
	OutDir         string `json:"outDir"`
	CookiesBrowser string `json:"cookiesBrowser"` // "chrome" | "edge" | "firefox" | ... | ""
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
	// Парсер прогресса разбит на части: при --concurrent-fragments yt-dlp
	// выводит строки в чуть других вариантах (frag-N, без ETA, без size и т.п.),
	// и единый строгий regex их не ловил — прогресс просто не показывался.
	progressRe     = regexp.MustCompile(`^\[download\]\s+([\d.]+)%`)
	progSizeRe     = regexp.MustCompile(`of\s+~?([\d.]+\s*\w+)`)
	progSpeedRe    = regexp.MustCompile(`at\s+([\d.]+\s*\w+/s)`)
	progEtaRe      = regexp.MustCompile(`ETA\s+(\S+)`)
	destRe         = regexp.MustCompile(`\[download\] Destination: (.+)`)
	mergeRe        = regexp.MustCompile(`\[Merger\]|Merging formats into`)
	mergerOutRe    = regexp.MustCompile(`Merging formats into "([^"]+)"`)
	alreadyDoneRe  = regexp.MustCompile(`\[download\]\s+(.+?)\s+has already been downloaded`)
	infoLineRe     = regexp.MustCompile(`^\[(youtube|info|ExtractAudio|VideoConvertor|FixupM3u8|Metadata)\]`)
	formatSuffixRe = regexp.MustCompile(`\.f\d+$`)

	// одиночные видео — всё остальное считаем каналом/плейлистом
	singleVideoRe = regexp.MustCompile(`(?i)(watch\?v=|youtu\.be/|/shorts/|/live/)`)
)

// archiveFileName — имя файла-архива yt-dlp в выбранной папке скачивания.
const archiveFileName = ".ytdl-archive.txt"

// IsInArchive проверяет, скачивалось ли видео с данным ID в указанную папку.
// Использует файл archiveFileName, который пишет yt-dlp через --download-archive.
func IsInArchive(outDir, videoID string) bool {
	if outDir == "" || videoID == "" {
		return false
	}
	data, err := os.ReadFile(filepath.Join(outDir, archiveFileName))
	if err != nil {
		return false
	}
	// Формат строк yt-dlp: "youtube VIDEOID" (или другой extractor)
	marker := " " + videoID
	for _, line := range strings.Split(string(data), "\n") {
		line = strings.TrimSpace(line)
		if line == "" {
			continue
		}
		if strings.HasSuffix(line, marker) || strings.Contains(line, " "+videoID+" ") {
			return true
		}
	}
	return false
}

// CheckDeps проверяет наличие yt-dlp и ffmpeg.
func CheckDeps() (ytdlpOK bool, ffmpegOK bool) {
	_, err := exec.LookPath("yt-dlp")
	ytdlpOK = err == nil
	_, err = exec.LookPath("ffmpeg")
	ffmpegOK = err == nil
	return
}

// GetVideoInfo получает информацию о видео через yt-dlp -J.
func GetVideoInfo(ctx context.Context, url string, cookiesBrowser string) (*VideoInfo, error) {
	args := []string{"--no-playlist", "--no-warnings", "-J"}
	if cookiesBrowser != "" {
		args = append(args, "--cookies-from-browser", cookiesBrowser)
	}
	args = append(args, url)

	cmd := exec.CommandContext(ctx, "yt-dlp", args...)
	hideConsole(cmd)

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
		"--no-overwrites",
		"--no-warnings",
		"--concurrent-fragments", "4",
		"--retries", "3",
		"--fragment-retries", "3",
		// Печатаем реальный путь к итоговому файлу после всех move/merge —
		// иначе [download] Destination указывает на промежуточный fXXX-файл,
		// который после merge перестаёт существовать.
		"--print", "after_move:FINAL_FILE:%(filepath)s",
		"-o", outputTemplate,
	}
	// Архив для пропуска уже скачанных видео по ID.
	if opts.OutDir != "" {
		args = append(args, "--download-archive", filepath.Join(opts.OutDir, archiveFileName))
	}
	if opts.CookiesBrowser != "" {
		args = append(args, "--cookies-from-browser", opts.CookiesBrowser)
	}

	// Превью:
	//  - всегда пишем .jpg (Windows нативно показывает jpg как иконку, webp — нет)
	//  - для mp4/mp3/m4a встраиваем как обложку; yt-dlp по умолчанию удаляет
	//    .jpg-файл после embed (--no-keep-thumbnail — это default), поэтому
	//    в основной папке мусора не остаётся
	//  - для webm/mkv embed нестабилен и часто ломает файл; .jpg остаётся
	//    жить в подпапке _thumbs/ рядом с видео, а не вперемешку с ним
	if opts.OutDir != "" {
		thumbTemplate := filepath.Join(opts.OutDir, "_thumbs", "%(title)s.%(ext)s")
		args = append(args, "-o", "thumbnail:"+thumbTemplate)
	}
	args = append(args,
		"--write-thumbnail",
		"--convert-thumbnails", "jpg",
	)
	// Не ставим --embed-thumbnail: для mp4 yt-dlp требует AtomicParsley или
	// mutagen (которых на типовом Windows нет) и тихо пропускает embed,
	// оставляя .jpg рядом. Делаем embed сами через ffmpeg в StartDownload.

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
	// Подпапка для превью webm/mkv (для mp4/mp3/m4a yt-dlp удалит файлы
	// после embed, но сама папка может остаться пустой — это норм).
	_ = os.MkdirAll(filepath.Join(outDir, "_thumbs"), 0o755)

	args := buildArgs(opts, outputTemplate)
	args = append(args, opts.URL)

	cmd := exec.CommandContext(ctx, "yt-dlp", args...)
	hideConsole(cmd)

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

	// Кооператив для отмены: exec.CommandContext.Kill убивает только сам
	// yt-dlp, оставляя дочерний ffmpeg. taskkill /T убивает всё дерево.
	pid := 0
	if cmd.Process != nil {
		pid = cmd.Process.Pid
	}
	killed := make(chan struct{})
	go func() {
		select {
		case <-ctx.Done():
			killProcessTree(pid)
		case <-killed:
		}
	}()
	defer close(killed)

	var lastFile string
	var alreadyDownloaded bool

	scanner := bufio.NewScanner(stdoutPipe)
	scanner.Buffer(make([]byte, 0, 64*1024), 1024*1024)
	for scanner.Scan() {
		line := scanner.Text()

		if m := progressRe.FindStringSubmatch(line); len(m) == 2 {
			pct, _ := strconv.ParseFloat(m[1], 64)
			p := Progress{Type: "progress", Percent: pct}
			if mm := progSizeRe.FindStringSubmatch(line); len(mm) == 2 {
				p.Size = strings.TrimSpace(mm[1])
			}
			if mm := progSpeedRe.FindStringSubmatch(line); len(mm) == 2 {
				p.Speed = strings.TrimSpace(mm[1])
			}
			if mm := progEtaRe.FindStringSubmatch(line); len(mm) == 2 {
				p.ETA = strings.TrimSpace(mm[1])
			}
			progressCh <- p
			continue
		}

		// Самое надёжное: yt-dlp печатает финальный путь после move/merge.
		if strings.HasPrefix(line, "FINAL_FILE:") {
			lastFile = strings.TrimSpace(strings.TrimPrefix(line, "FINAL_FILE:"))
			continue
		}

		if m := destRe.FindStringSubmatch(line); len(m) == 2 {
			// Это может быть промежуточный fXXX-файл, но FINAL_FILE:
			// или Merger строка прилетит позже и перезапишет.
			lastFile = strings.TrimSpace(m[1])
			continue
		}

		if m := alreadyDoneRe.FindStringSubmatch(line); len(m) == 2 {
			lastFile = strings.TrimSpace(m[1])
			alreadyDownloaded = true
			continue
		}

		if m := mergerOutRe.FindStringSubmatch(line); len(m) == 2 {
			lastFile = strings.TrimSpace(m[1])
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
			continue
		}

		// Промежуточные сообщения yt-dlp (extract info, postprocessing, etc.) —
		// показываем как статус, чтобы пользователь видел, что идёт работа.
		if infoLineRe.MatchString(line) {
			progressCh <- Progress{Type: "info", Message: line}
		}
	}

	waitErr := cmd.Wait()
	// Отмена пользователем: убираем за собой недокачанные .part / .ytdl /
	// промежуточные fXXX-файлы и временные обложки, чтобы папка осталась чистой.
	if ctx.Err() != nil {
		cleanupPartials(outDir, lastFile)
		return
	}
	if waitErr != nil {
		errMsg := strings.TrimSpace(stderrBuf.String())
		if errMsg == "" {
			errMsg = waitErr.Error()
		}
		progressCh <- Progress{Type: "error", Message: errMsg}
		return
	}

	// Если файл сохранён под другим именем (после post-processing), попробуем найти.
	finalFile := lastFile
	if finalFile != "" {
		if _, err := os.Stat(finalFile); err != nil {
			// Промежуточные файлы yt-dlp имеют суффикс .fNNN перед расширением
			// (напр. "Video.f137.mp4") — убираем, чтобы получить финальное имя.
			base := strings.TrimSuffix(finalFile, filepath.Ext(finalFile))
			base = formatSuffixRe.ReplaceAllString(base, "")
			for _, ext := range []string{".mp4", ".webm", ".mkv", ".mp3", ".m4a", ".opus"} {
				candidate := base + ext
				if _, err := os.Stat(candidate); err == nil {
					finalFile = candidate
					break
				}
			}
		}
	}

	// Embed-им обложку сами через ffmpeg. Не пытаемся угадывать имя итогового
	// файла из вывода yt-dlp (там бывают промежуточные fXXX, merge-строки,
	// разные форматы под разными версиями) — просто проходим по папке
	// и для каждого медиа-файла, у которого ЕСТЬ соответствующий .jpg
	// (рядом или в _thumbs/), делаем embed. Уже обработанные пропускаются
	// автоматически — у них .jpg уже удалён.
	embedThumbnailsInDir(outDir)

	doneMsg := "Загрузка завершена!"
	if alreadyDownloaded {
		doneMsg = "Уже скачано — пропущено"
	}
	progressCh <- Progress{
		Type:     "done",
		Percent:  100,
		Message:  doneMsg,
		FilePath: finalFile,
	}
}

// ─── Channel / Playlist ───────────────────────────────────────────────────────

// ChannelEntry — одно видео из канала или плейлиста.
type ChannelEntry struct {
	ID        string
	URL       string
	Title     string
	Duration  int
	Thumbnail string
}

// ChannelInfo — метаданные канала/плейлиста вместе со списком видео.
type ChannelInfo struct {
	Title    string
	Uploader string
	Entries  []ChannelEntry
}

// BatchProgress — событие прогресса последовательной батч-загрузки.
type BatchProgress struct {
	// "start" | "progress" | "merge" | "video_done" | "video_error" | "batch_done"
	Type       string
	Index      int      // текущий индекс (0-based)
	Total      int      // всего видео в батче
	VideoTitle string
	Inner      Progress // вложенное событие от StartDownload
}

// внутренние структуры для парсинга JSON от yt-dlp --flat-playlist -J
type ytDLPPlaylistRaw struct {
	Title    string          `json:"title"`
	Uploader string          `json:"uploader"`
	Channel  string          `json:"channel"`
	Entries  []ytDLPEntryRaw `json:"entries"`
}

type ytDLPEntryRaw struct {
	ID         string  `json:"id"`
	URL        string  `json:"url"`
	WebpageURL string  `json:"webpage_url"`
	Title      string  `json:"title"`
	Duration   float64 `json:"duration"`
	Thumbnail  string  `json:"thumbnail"`
}

// IsChannelURL возвращает true если URL ведёт на канал или плейлист.
func IsChannelURL(rawURL string) bool {
	return !singleVideoRe.MatchString(rawURL)
}

// GetChannelVideos получает страницу видео из канала/плейлиста.
// offset — сколько видео пропустить (0-based), limit — сколько взять.
func GetChannelVideos(ctx context.Context, url string, offset, limit int, cookiesBrowser string) (*ChannelInfo, error) {
	start := offset + 1
	end := offset + limit
	itemSpec := fmt.Sprintf("%d:%d", start, end)

	args := []string{"--flat-playlist", "--no-warnings", "-I", itemSpec, "-J"}
	if cookiesBrowser != "" {
		args = append(args, "--cookies-from-browser", cookiesBrowser)
	}
	args = append(args, url)

	cmd := exec.CommandContext(ctx, "yt-dlp", args...)
	hideConsole(cmd)

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

	var raw ytDLPPlaylistRaw
	if err := json.Unmarshal(stdout.Bytes(), &raw); err != nil {
		return nil, fmt.Errorf("не удалось разобрать плейлист: %w", err)
	}

	uploader := raw.Uploader
	if uploader == "" {
		uploader = raw.Channel
	}

	entries := make([]ChannelEntry, 0, len(raw.Entries))
	for _, e := range raw.Entries {
		// предпочитаем каноничный URL по ID — yt-dlp иногда отдаёт в url только id
		var entryURL string
		switch {
		case e.ID != "":
			entryURL = "https://www.youtube.com/watch?v=" + e.ID
		case e.WebpageURL != "":
			entryURL = e.WebpageURL
		default:
			entryURL = e.URL
		}

		thumb := e.Thumbnail
		if thumb == "" && e.ID != "" {
			thumb = "https://i.ytimg.com/vi/" + e.ID + "/hqdefault.jpg"
		}

		entries = append(entries, ChannelEntry{
			ID:        e.ID,
			URL:       entryURL,
			Title:     e.Title,
			Duration:  int(e.Duration),
			Thumbnail: thumb,
		})
	}

	return &ChannelInfo{
		Title:    raw.Title,
		Uploader: uploader,
		Entries:  entries,
	}, nil
}

// DownloadBatch последовательно скачивает переданные видео.
// При ошибке одного видео отправляет "video_error" и переходит к следующему.
// Отмена ctx прерывает весь батч после текущего видео.
func DownloadBatch(ctx context.Context, entries []ChannelEntry, opts DownloadOptions, ch chan<- BatchProgress) {
	defer close(ch)

	total := len(entries)
	for i, entry := range entries {
		if ctx.Err() != nil {
			return
		}

		ch <- BatchProgress{
			Type:       "start",
			Index:      i,
			Total:      total,
			VideoTitle: entry.Title,
		}

		// Быстрый пропуск по архиву: не запускаем yt-dlp вовсе, экономим минуты
		// на вытаскивании метаданных каждого видео.
		if IsInArchive(opts.OutDir, entry.ID) {
			ch <- BatchProgress{
				Type:       "video_done",
				Index:      i,
				Total:      total,
				VideoTitle: entry.Title,
				Inner: Progress{
					Type:    "done",
					Percent: 100,
					Message: "Уже скачано — пропущено",
				},
			}
			continue
		}

		innerOpts := opts
		innerOpts.URL = entry.URL

		innerCh := make(chan Progress, 64)
		go StartDownload(ctx, innerOpts, innerCh)

		for p := range innerCh {
			evType := p.Type
			switch p.Type {
			case "done":
				evType = "video_done"
			case "error":
				evType = "video_error"
			}
			ch <- BatchProgress{
				Type:       evType,
				Index:      i,
				Total:      total,
				VideoTitle: entry.Title,
				Inner:      p,
			}
		}

		// контекст отменён — прерываем батч
		if ctx.Err() != nil {
			return
		}
	}

	ch <- BatchProgress{Type: "batch_done", Total: total}
}

// ─── Thumbnail embedding (через ffmpeg) ───────────────────────────────────────

// cleanupPartials удаляет следы прерванной загрузки в outDir: .part / .ytdl,
// промежуточные fNNN-файлы (Video.f137.mp4, Video.f140.m4a) и временные
// embed_tmp. lastFile, если непустой, помогает таргетированно подчистить
// именно текущую загрузку. Если lastFile неизвестен — чистим все .part/.ytdl
// в outDir (других открытых загрузок нет, так как у нас всегда одна за раз).
func cleanupPartials(outDir, lastFile string) {
	if outDir == "" {
		return
	}

	// 1. Глобальная зачистка .part / .ytdl / .embed_tmp в outDir.
	if entries, err := os.ReadDir(outDir); err == nil {
		for _, e := range entries {
			if e.IsDir() {
				continue
			}
			name := strings.ToLower(e.Name())
			if strings.HasSuffix(name, ".part") ||
				strings.HasSuffix(name, ".ytdl") ||
				strings.Contains(name, ".embed_tmp.") {
				_ = os.Remove(filepath.Join(outDir, e.Name()))
			}
		}
	}

	// 2. Если знаем имя текущей загрузки — снесём промежуточные fNNN-файлы.
	if lastFile != "" {
		base := strings.TrimSuffix(filepath.Base(lastFile), filepath.Ext(lastFile))
		base = formatSuffixRe.ReplaceAllString(base, "")
		matches, _ := filepath.Glob(filepath.Join(outDir, base+".f*.*"))
		for _, m := range matches {
			_ = os.Remove(m)
		}
		// Превью этой же загрузки — тоже мусор, целевого файла нет.
		for _, dir := range []string{outDir, filepath.Join(outDir, "_thumbs")} {
			for _, ext := range []string{".jpg", ".jpeg", ".png", ".webp"} {
				_ = os.Remove(filepath.Join(dir, base+ext))
			}
		}
	}

	// 3. Удаляем пустую _thumbs/, если осталась.
	thumbsDir := filepath.Join(outDir, "_thumbs")
	if entries, err := os.ReadDir(thumbsDir); err == nil && len(entries) == 0 {
		_ = os.Remove(thumbsDir)
	}
}

// embedThumbnailsInDir сканирует папку, для каждого .mp4/.m4a/.mp3 ищет
// одноимённый .jpg (в _thumbs/ или рядом) и embed-ит его через ffmpeg.
// Уже обработанные пропускаются автоматически — у них .jpg отсутствует.
// В конце удаляет пустую _thumbs/.
func embedThumbnailsInDir(outDir string) {
	if outDir == "" {
		return
	}
	entries, err := os.ReadDir(outDir)
	if err != nil {
		return
	}
	for _, e := range entries {
		if e.IsDir() {
			continue
		}
		ext := strings.ToLower(filepath.Ext(e.Name()))
		if ext != ".mp4" && ext != ".m4a" && ext != ".mp3" {
			continue
		}
		mediaPath := filepath.Join(outDir, e.Name())
		thumb := findThumbnail(outDir, mediaPath)
		if thumb == "" {
			continue
		}
		if err := embedThumbnail(mediaPath, thumb); err == nil {
			_ = os.Remove(thumb)
		}
	}
	thumbsDir := filepath.Join(outDir, "_thumbs")
	if leftover, err := os.ReadDir(thumbsDir); err == nil && len(leftover) == 0 {
		_ = os.Remove(thumbsDir)
	}
}

// findThumbnail ищет .jpg-превью для скачанного файла. Сначала смотрит в
// _thumbs/ (куда мы направляем yt-dlp через -o thumbnail:...), затем рядом
// с самим файлом — на случай если yt-dlp по какой-то причине положил туда.
func findThumbnail(outDir, mediaPath string) string {
	base := strings.TrimSuffix(filepath.Base(mediaPath), filepath.Ext(mediaPath))
	candidates := []string{
		filepath.Join(outDir, "_thumbs", base+".jpg"),
		filepath.Join(outDir, "_thumbs", base+".jpeg"),
		filepath.Join(outDir, "_thumbs", base+".png"),
		filepath.Join(outDir, "_thumbs", base+".webp"),
		filepath.Join(outDir, base+".jpg"),
		filepath.Join(outDir, base+".jpeg"),
		filepath.Join(outDir, base+".png"),
		filepath.Join(outDir, base+".webp"),
	}
	for _, c := range candidates {
		if _, err := os.Stat(c); err == nil {
			return c
		}
	}
	return ""
}

// embedThumbnail встраивает обложку в mp4/m4a/mp3 через ffmpeg.
// Запись идёт во временный файл, потом атомарно переименовывается в исходный —
// при ошибке исходник остаётся целым.
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
		return fmt.Errorf("embed not supported for %s", ext)
	}

	cmd := exec.Command("ffmpeg", args...)
	hideConsole(cmd)
	var stderr bytes.Buffer
	cmd.Stderr = &stderr
	if err := cmd.Run(); err != nil {
		_ = os.Remove(tmp)
		msg := strings.TrimSpace(stderr.String())
		if msg == "" {
			msg = err.Error()
		}
		return fmt.Errorf("ffmpeg embed: %s", msg)
	}
	return os.Rename(tmp, mediaPath)
}
