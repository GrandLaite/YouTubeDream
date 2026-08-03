package main

import (
	"context"
	"fmt"
	"image"
	_ "image/jpeg"
	_ "image/png"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"time"

	"fyne.io/fyne/v2"
	"fyne.io/fyne/v2/app"
	"fyne.io/fyne/v2/canvas"
	"fyne.io/fyne/v2/container"
	"fyne.io/fyne/v2/dialog"
	"fyne.io/fyne/v2/storage"
	"fyne.io/fyne/v2/widget"
)

func main() {
	os.Setenv("FYNE_THEME", "dark")

	a := app.NewWithID("com.ytdl.app")
	w := a.NewWindow("YT Downloader")
	w.Resize(fyne.NewSize(860, 560))
	w.SetMaster()

	ui := newUI(a, w)
	w.SetContent(ui.build())
	w.ShowAndRun()
}

// ─── UI struct ────────────────────────────────────────────────────────────────

type UI struct {
	app fyne.App
	win fyne.Window

	// URL
	urlEntry     *widget.Entry
	fetchBtn     *widget.Button
	fetchProg    *widget.ProgressBarInfinite
	cookiesSel   *widget.Select
	rootDirEntry *widget.Entry // целевая папка верхнего уровня (всегда видна в шапке)

	// Info
	thumbImg      *canvas.Image
	titleLabel    *widget.Label
	uploaderLabel *widget.Label
	durationLabel *widget.Label
	infoCard      *widget.Card

	// Options (single video)
	qualitySel  *widget.Select
	formatSel   *widget.Select
	hintLabel   *widget.Label
	outdirEntry *widget.Entry
	downloadBtn *widget.Button
	optCard     *widget.Card

	// Channel / playlist mode
	channelCard    *widget.Card
	channelListBox *fyne.Container
	channelChecks  []*widget.Check
	channelThumbs  []*canvas.Image
	channelEntries []ChannelEntry
	channelOffset  int
	channelURL     string
	loadMoreBtn    *widget.Button

	// Batch options
	batchOptCard     *widget.Card
	batchQualitySel  *widget.Select
	batchFormatSel   *widget.Select
	batchOutdirEntry *widget.Entry

	// Progress
	progBar    *widget.ProgressBar
	progStatus *widget.Label
	progPct    *widget.Label
	speedLabel *widget.Label
	etaLabel   *widget.Label
	sizeLabel  *widget.Label
	cancelBtn  *widget.Button
	openDirBtn *widget.Button
	newDlBtn   *widget.Button
	progCard   *widget.Card

	// State
	videoInfo        *VideoInfo
	qualityItems     []Quality
	cancelFn         context.CancelFunc
	outDir           string // корневая папка (где живут папки каналов)
	lastFilePath     string
	isBatchMode      bool
	activeOutDir     string
	batchFailedCount int
	batchFailedMsgs  []string
	suppressRootSync bool // подавляет OnChanged-callback при программном SetText
}

// Ключи Fyne Preferences.
const (
	prefKeyOutDir  = "outDir"
	prefKeyCookies = "cookiesBrowser"
)

func newUI(a fyne.App, w fyne.Window) *UI {
	home, _ := os.UserHomeDir()
	defaultDir := filepath.Join(home, "Downloads")
	saved := a.Preferences().StringWithFallback(prefKeyOutDir, defaultDir)
	return &UI{
		app:    a,
		win:    w,
		outDir: saved,
	}
}

// rememberOutDir сохраняет выбранную папку в Fyne Preferences и в state UI,
// чтобы она восстанавливалась при следующем запуске и переиспользовалась
// между single/batch режимами.
func (u *UI) rememberOutDir(dir string) {
	dir = strings.TrimSpace(dir)
	if dir == "" {
		return
	}
	u.outDir = dir
	u.app.Preferences().SetString(prefKeyOutDir, dir)
}

// setRootDir вызывается при выборе папки через диалог — обновляет шапку,
// синхронизирует single/batch entries и запоминает в prefs.
func (u *UI) setRootDir(dir string) {
	dir = strings.TrimSpace(dir)
	if dir == "" {
		return
	}
	u.rememberOutDir(dir)
	u.suppressRootSync = true
	u.rootDirEntry.SetText(dir)
	u.suppressRootSync = false
	u.syncFromRoot(dir)
}

// syncFromRoot обновляет вспомогательные dir-entries в single/batch cards,
// если они уже созданы. Программная установка не должна триггерить их
// OnChanged-сохранение — но мы и убрали OnChanged-rememberOutDir с них.
func (u *UI) syncFromRoot(dir string) {
	if u.outdirEntry != nil {
		u.outdirEntry.SetText(dir)
	}
	if u.batchOutdirEntry != nil {
		u.batchOutdirEntry.SetText(dir)
	}
}

// setDialogStartDir заставляет диалог выбора папки открыться в u.outDir,
// иначе Fyne всегда стартует с домашней директории.
func (u *UI) setDialogStartDir(fd *dialog.FileDialog) {
	dir := strings.TrimSpace(u.outDir)
	if dir == "" {
		return
	}
	if _, err := os.Stat(dir); err != nil {
		return
	}
	abs, err := filepath.Abs(dir)
	if err != nil {
		return
	}
	lister, err := storage.ListerForURI(storage.NewFileURI(abs))
	if err != nil {
		return
	}
	fd.SetLocation(lister)
}

// sanitizeFolderName убирает символы, недопустимые в именах папок Windows.
func sanitizeFolderName(s string) string {
	bad := []string{`<`, `>`, `:`, `"`, `/`, `\`, `|`, `?`, `*`}
	for _, b := range bad {
		s = strings.ReplaceAll(s, b, "_")
	}
	s = strings.TrimSpace(s)
	s = strings.Trim(s, ".")
	if s == "" {
		s = "channel"
	}
	return s
}

// ─── Build ────────────────────────────────────────────────────────────────────

func (u *UI) build() fyne.CanvasObject {
	// ── URL section ──────────────────────────────────
	u.urlEntry = widget.NewEntry()
	u.urlEntry.SetPlaceHolder("https://www.youtube.com/watch?v=…")
	u.urlEntry.OnSubmitted = func(_ string) { u.doFetch() }

	u.fetchBtn = widget.NewButton("Получить", u.doFetch)
	u.fetchBtn.Importance = widget.HighImportance

	u.fetchProg = widget.NewProgressBarInfinite()
	u.fetchProg.Hide()

	urlRow := container.NewBorder(nil, nil, nil, u.fetchBtn, u.urlEntry)

	u.cookiesSel = widget.NewSelect(
		[]string{"Без cookies", "Chrome", "Edge", "Firefox", "Brave", "Opera", "Vivaldi", "Chromium"},
		func(s string) { u.app.Preferences().SetString(prefKeyCookies, s) },
	)
	u.cookiesSel.SetSelected(u.app.Preferences().StringWithFallback(prefKeyCookies, "Без cookies"))

	cookiesRow := container.NewBorder(
		nil, nil,
		widget.NewLabel("Cookies из браузера:"),
		nil,
		u.cookiesSel,
	)

	// Целевая папка в шапке — задаётся ДО ввода канала, чтобы диалог
	// "создать папку канала" предлагал именно её, а не подпапку прошлого канала.
	u.rootDirEntry = widget.NewEntry()
	u.rootDirEntry.SetText(u.outDir)
	u.rootDirEntry.SetPlaceHolder("~/Downloads")
	rootBrowseBtn := widget.NewButton("…", func() {
		fd := dialog.NewFolderOpen(func(uri fyne.ListableURI, err error) {
			if err != nil || uri == nil {
				return
			}
			u.setRootDir(uri.Path())
		}, u.win)
		u.setDialogStartDir(fd)
		fd.Show()
	})
	// Запоминаем только при ручном вводе пользователя — программные изменения
	// (синк из других полей) сюда не попадают.
	u.rootDirEntry.OnChanged = func(s string) {
		if !u.suppressRootSync {
			u.rememberOutDir(s)
			u.syncFromRoot(s)
		}
	}
	rootDirRow := container.NewBorder(
		nil, nil,
		widget.NewLabel("Целевая папка:"),
		rootBrowseBtn,
		u.rootDirEntry,
	)

	urlCard := widget.NewCard("", "", container.NewVBox(urlRow, cookiesRow, rootDirRow, u.fetchProg))

	// ── Info card ─────────────────────────────────────
	placeholder := image.NewNRGBA(image.Rect(0, 0, 192, 108))
	u.thumbImg = canvas.NewImageFromImage(placeholder)
	u.thumbImg.FillMode = canvas.ImageFillContain
	u.thumbImg.SetMinSize(fyne.NewSize(192, 108))

	u.titleLabel = widget.NewLabelWithStyle("", fyne.TextAlignLeading, fyne.TextStyle{Bold: true})
	u.titleLabel.Wrapping = fyne.TextWrapWord

	u.uploaderLabel = widget.NewLabel("")
	u.durationLabel = widget.NewLabel("")

	infoText := container.NewVBox(u.titleLabel, u.uploaderLabel, u.durationLabel)
	infoRow := container.NewBorder(nil, nil, u.thumbImg, nil, infoText)
	u.infoCard = widget.NewCard("", "", infoRow)
	u.infoCard.Hide()

	// ── Options card ──────────────────────────────────
	u.qualitySel = widget.NewSelect(nil, u.onQualityChanged)
	u.qualitySel.PlaceHolder = "— выберите качество —"

	// hintLabel создаём ДО formatSel, потому что SetSelected сразу стреляет коллбэком.
	u.hintLabel = widget.NewLabelWithStyle("", fyne.TextAlignLeading, fyne.TextStyle{Italic: true})
	u.hintLabel.Wrapping = fyne.TextWrapWord

	u.formatSel = widget.NewSelect([]string{"MP4", "WebM", "MKV", "MP3", "M4A"}, u.onFormatChanged)
	u.formatSel.SetSelected("MP4")

	u.outdirEntry = widget.NewEntry()
	u.outdirEntry.SetText(u.outDir)
	u.outdirEntry.SetPlaceHolder("~/Downloads")

	browseBtn := widget.NewButton("…", func() {
		fd := dialog.NewFolderOpen(func(uri fyne.ListableURI, err error) {
			if err != nil || uri == nil {
				return
			}
			u.setRootDir(uri.Path())
		}, u.win)
		u.setDialogStartDir(fd)
		fd.Show()
	})
	dirRow := container.NewBorder(nil, nil, nil, browseBtn, u.outdirEntry)

	u.downloadBtn = widget.NewButton("⬇  Скачать", u.doDownload)
	u.downloadBtn.Importance = widget.HighImportance

	selGrid := container.NewGridWithColumns(2,
		container.NewVBox(widget.NewLabel("Качество"), u.qualitySel),
		container.NewVBox(widget.NewLabel("Контейнер"), u.formatSel),
	)
	optContent := container.NewVBox(
		selGrid,
		u.hintLabel,
		widget.NewSeparator(),
		container.NewVBox(widget.NewLabel("Папка сохранения"), dirRow),
		u.downloadBtn,
	)
	u.optCard = widget.NewCard("", "", optContent)
	u.optCard.Hide()

	// ── Channel card ─────────────────────────────────
	u.channelListBox = container.NewVBox()
	listScroll := container.NewScroll(u.channelListBox)
	listScroll.SetMinSize(fyne.NewSize(0, 380))

	selectAllBtn := widget.NewButton("Выбрать все", func() {
		for _, ch := range u.channelChecks {
			ch.SetChecked(true)
		}
	})
	deselectAllBtn := widget.NewButton("Снять все", func() {
		for _, ch := range u.channelChecks {
			ch.SetChecked(false)
		}
	})
	selActionsRow := container.NewHBox(selectAllBtn, deselectAllBtn)

	u.loadMoreBtn = widget.NewButton("Загрузить ещё 30", u.doLoadMore)
	u.loadMoreBtn.Hide()

	channelContent := container.NewVBox(
		selActionsRow,
		widget.NewSeparator(),
		listScroll,
		u.loadMoreBtn,
	)
	u.channelCard = widget.NewCard("", "", channelContent)
	u.channelCard.Hide()

	// ── Batch options card ────────────────────────────
	// batchFormatSel создаём ДО SetSelected на batchQualitySel — коллбэк стреляет немедленно.
	u.batchFormatSel = widget.NewSelect([]string{"MP4", "WebM", "MKV"}, nil)
	u.batchFormatSel.SetSelected("MP4")

	u.batchQualitySel = widget.NewSelect(
		[]string{"Лучшее", "1080p", "720p", "480p", "360p", "Только аудио"},
		u.onBatchQualityChanged,
	)
	u.batchQualitySel.SetSelected("Лучшее")

	u.batchOutdirEntry = widget.NewEntry()
	u.batchOutdirEntry.SetText(u.outDir)
	u.batchOutdirEntry.SetPlaceHolder("~/Downloads")

	batchBrowseBtn := widget.NewButton("…", func() {
		fd := dialog.NewFolderOpen(func(uri fyne.ListableURI, err error) {
			if err != nil || uri == nil {
				return
			}
			u.setRootDir(uri.Path())
		}, u.win)
		u.setDialogStartDir(fd)
		fd.Show()
	})
	batchDirRow := container.NewBorder(nil, nil, nil, batchBrowseBtn, u.batchOutdirEntry)

	batchDownloadBtn := widget.NewButton("⬇  Скачать выбранные", u.doBatchDownload)
	batchDownloadBtn.Importance = widget.HighImportance

	batchSelGrid := container.NewGridWithColumns(2,
		container.NewVBox(widget.NewLabel("Качество"), u.batchQualitySel),
		container.NewVBox(widget.NewLabel("Контейнер"), u.batchFormatSel),
	)
	batchOptContent := container.NewVBox(
		batchSelGrid,
		widget.NewSeparator(),
		container.NewVBox(widget.NewLabel("Папка сохранения"), batchDirRow),
		batchDownloadBtn,
	)
	u.batchOptCard = widget.NewCard("", "", batchOptContent)
	u.batchOptCard.Hide()

	// ── Progress card ─────────────────────────────────
	u.progBar = widget.NewProgressBar()
	u.progPct = widget.NewLabelWithStyle("0%", fyne.TextAlignCenter, fyne.TextStyle{Bold: true})
	u.progStatus = widget.NewLabel("")
	u.speedLabel = widget.NewLabel("")
	u.etaLabel = widget.NewLabel("")
	u.sizeLabel = widget.NewLabel("")

	statsRow := container.NewGridWithColumns(3, u.sizeLabel, u.speedLabel, u.etaLabel)

	u.cancelBtn = widget.NewButton("Отмена", u.doCancel)
	u.openDirBtn = widget.NewButton("Открыть папку", u.doOpenDir)
	u.openDirBtn.Hide()
	u.newDlBtn = widget.NewButton("Скачать ещё", u.doNewDownload)
	u.newDlBtn.Hide()

	actionsRow := container.NewHBox(u.cancelBtn, u.openDirBtn, u.newDlBtn)

	progContent := container.NewVBox(
		container.NewBorder(nil, nil, u.progPct, nil, u.progBar),
		u.progStatus,
		statsRow,
		actionsRow,
	)
	u.progCard = widget.NewCard("Загрузка", "", progContent)
	u.progCard.Hide()

	// ── Assemble ──────────────────────────────────────
	body := container.NewVBox(urlCard, u.infoCard, u.optCard, u.channelCard, u.batchOptCard, u.progCard)
	scroll := container.NewScroll(body)

	return container.NewPadded(scroll)
}

// cookiesBrowser возвращает значение для --cookies-from-browser или "" если выключено.
func (u *UI) cookiesBrowser() string {
	switch u.cookiesSel.Selected {
	case "Chrome":
		return "chrome"
	case "Edge":
		return "edge"
	case "Firefox":
		return "firefox"
	case "Brave":
		return "brave"
	case "Opera":
		return "opera"
	case "Vivaldi":
		return "vivaldi"
	case "Chromium":
		return "chromium"
	default:
		return ""
	}
}

// ─── Fetch ────────────────────────────────────────────────────────────────────

func (u *UI) doFetch() {
	rawURL := strings.TrimSpace(u.urlEntry.Text)
	if rawURL == "" {
		dialog.ShowError(fmt.Errorf("введите ссылку"), u.win)
		return
	}

	u.fetchBtn.Disable()
	u.fetchProg.Show()
	u.infoCard.Hide()
	u.optCard.Hide()
	u.channelCard.Hide()
	u.batchOptCard.Hide()
	u.progCard.Hide()
	u.isBatchMode = false

	if IsChannelURL(rawURL) {
		u.doFetchChannel(rawURL)
		return
	}

	go func() {
		ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
		defer cancel()

		info, err := GetVideoInfo(ctx, rawURL, u.cookiesBrowser())

		u.fetchProg.Hide()
		u.fetchBtn.Enable()

		if err != nil {
			dialog.ShowError(err, u.win)
			return
		}

		u.videoInfo = info
		u.renderInfo(info)
	}()
}

func (u *UI) doFetchChannel(rawURL string) {
	u.channelURL = rawURL
	u.channelEntries = nil
	u.channelChecks = nil
	u.channelThumbs = nil
	u.channelOffset = 0
	u.channelListBox.RemoveAll()

	go func() {
		ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
		defer cancel()

		info, err := GetChannelVideos(ctx, rawURL, 0, 30, u.cookiesBrowser())

		u.fetchProg.Hide()
		u.fetchBtn.Enable()

		if err != nil {
			dialog.ShowError(err, u.win)
			return
		}

		u.renderChannel(info)
	}()
}

func (u *UI) renderChannel(info *ChannelInfo) {
	title := info.Title
	if title == "" {
		title = info.Uploader
	}
	u.channelCard.SetTitle(title)

	u.appendChannelEntries(info.Entries)
	u.channelOffset = len(info.Entries)

	if len(info.Entries) >= 30 {
		u.loadMoreBtn.Show()
	} else {
		u.loadMoreBtn.Hide()
	}

	u.isBatchMode = true
	u.channelCard.Show()
	u.batchOptCard.Show()

	// Спрашиваем, создавать ли подпапку с названием канала.
	folderName := info.Uploader
	if folderName == "" {
		folderName = info.Title
	}
	u.askChannelSubfolder(folderName)
}

// askChannelSubfolder показывает диалог "создать подпапку для канала?".
// База — ВСЕГДА корневая u.outDir (из шапки), а не текущее значение
// batchOutdirEntry — иначе при последовательной загрузке двух каналов
// диалог предлагал бы создать "Канал2" внутри "Канал1".
func (u *UI) askChannelSubfolder(channelName string) {
	channelName = sanitizeFolderName(channelName)
	if channelName == "" || channelName == "channel" {
		return
	}
	base := strings.TrimSpace(u.outDir)
	if base == "" {
		base = strings.TrimSpace(u.rootDirEntry.Text)
	}
	if base == "" {
		return
	}
	target := filepath.Join(base, channelName)

	// Если корневая уже совпадает — подпапка не нужна.
	if filepath.Base(base) == channelName {
		u.batchOutdirEntry.SetText(base)
		return
	}

	msg := fmt.Sprintf(
		"Создать папку «%s» в\n%s\nи скачать все видео канала туда?",
		channelName, base,
	)
	dialog.ShowConfirm("Папка для канала", msg, func(ok bool) {
		if !ok {
			// Отказались от подпапки — качаем прямо в корневую.
			u.batchOutdirEntry.SetText(base)
			return
		}
		if err := os.MkdirAll(target, 0o755); err != nil {
			dialog.ShowError(fmt.Errorf("не удалось создать папку: %w", err), u.win)
			return
		}
		u.batchOutdirEntry.SetText(target)
	}, u.win)
}

func (u *UI) appendChannelEntries(entries []ChannelEntry) {
	for i, entry := range entries {
		idx := len(u.channelEntries) + i
		row := u.buildVideoRow(idx, entry)
		u.channelListBox.Add(row)
	}
	u.channelEntries = append(u.channelEntries, entries...)
	u.channelListBox.Refresh()
}

func (u *UI) buildVideoRow(idx int, entry ChannelEntry) fyne.CanvasObject {
	check := widget.NewCheck("", nil)
	check.SetChecked(true)
	u.channelChecks = append(u.channelChecks, check)

	placeholder := image.NewNRGBA(image.Rect(0, 0, 80, 45))
	thumb := canvas.NewImageFromImage(placeholder)
	thumb.FillMode = canvas.ImageFillContain
	thumb.SetMinSize(fyne.NewSize(80, 45))
	u.channelThumbs = append(u.channelThumbs, thumb)

	if entry.Thumbnail != "" {
		go func() {
			resp, err := http.Get(entry.Thumbnail)
			if err != nil {
				return
			}
			defer resp.Body.Close()
			img, _, err := image.Decode(resp.Body)
			if err != nil {
				return
			}
			thumb.Image = img
			canvas.Refresh(thumb)
		}()
	}

	numLabel := widget.NewLabel(fmt.Sprintf("%d.", idx+1))
	numLabel.Alignment = fyne.TextAlignTrailing

	titleLabel := widget.NewLabel(entry.Title)
	titleLabel.Wrapping = fyne.TextWrapWord

	durLabel := widget.NewLabel(formatDuration(entry.Duration))

	textCol := container.NewBorder(nil, nil, nil, durLabel, titleLabel)
	row := container.NewBorder(nil, nil,
		container.NewHBox(check, numLabel, thumb),
		nil,
		textCol,
	)
	return container.NewPadded(row)
}

func (u *UI) doLoadMore() {
	u.loadMoreBtn.Disable()

	go func() {
		ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
		defer cancel()

		info, err := GetChannelVideos(ctx, u.channelURL, u.channelOffset, 30, u.cookiesBrowser())

		u.loadMoreBtn.Enable()

		if err != nil {
			dialog.ShowError(err, u.win)
			return
		}

		u.appendChannelEntries(info.Entries)
		u.channelOffset += len(info.Entries)

		if len(info.Entries) < 30 {
			u.loadMoreBtn.Hide()
		}
	}()
}

func (u *UI) renderInfo(info *VideoInfo) {
	u.titleLabel.SetText(info.Title)
	u.uploaderLabel.SetText(info.Uploader)
	u.durationLabel.SetText("⏱  " + formatDuration(info.Duration))

	// Thumbnail в фоне
	if info.Thumbnail != "" {
		go func() {
			resp, err := http.Get(info.Thumbnail)
			if err != nil {
				return
			}
			defer resp.Body.Close()
			img, _, err := image.Decode(resp.Body)
			if err != nil {
				return
			}
			u.thumbImg.Image = img
			canvas.Refresh(u.thumbImg)
		}()
	}

	// Качества
	u.qualityItems = make([]Quality, 0, len(info.Qualities)+1)
	opts := make([]string, 0, len(info.Qualities)+1)
	for _, q := range info.Qualities {
		u.qualityItems = append(u.qualityItems, q)
		opts = append(opts, q.Label)
	}
	u.qualityItems = append(u.qualityItems, Quality{Height: 0, Label: "Только аудио"})
	opts = append(opts, "Только аудио")

	u.qualitySel.Options = opts
	if len(opts) > 0 {
		u.qualitySel.SetSelected(opts[0])
	}
	u.qualitySel.Refresh()

	u.syncFormatOptions()
	u.updateHint()

	u.infoCard.Show()
	u.optCard.Show()
}

// ─── Options logic ────────────────────────────────────────────────────────────

func (u *UI) onQualityChanged(_ string) {
	u.syncFormatOptions()
	u.updateHint()
}

func (u *UI) onFormatChanged(_ string) {
	u.updateHint()
}

func (u *UI) syncFormatOptions() {
	if u.selectedQualityCode() == "audio" {
		u.formatSel.Options = []string{"MP3", "M4A"}
		if u.formatSel.Selected != "MP3" && u.formatSel.Selected != "M4A" {
			u.formatSel.SetSelected("MP3")
		}
	} else {
		u.formatSel.Options = []string{"MP4", "WebM", "MKV"}
		if u.formatSel.Selected == "MP3" || u.formatSel.Selected == "M4A" {
			u.formatSel.SetSelected("MP4")
		}
	}
	u.formatSel.Refresh()
}

func (u *UI) updateHint() {
	hints := map[string]string{
		"MP4":  "H.264 + AAC → мультиплексирование без перекодирования. Максимальная совместимость.",
		"WebM": "VP9/AV1 + Opus → нативные кодеки YouTube, слияние без потерь качества.",
		"MKV":  "Лучшее качество: любые кодеки без перекодирования, широкая поддержка плеерами.",
		"MP3":  "Аудио конвертируется в MP3 (небольшая потеря качества).",
		"M4A":  "Нативный AAC из YouTube — без перекодирования, лучшее качество аудио.",
	}
	u.hintLabel.SetText(hints[u.formatSel.Selected])
}

func (u *UI) selectedQualityCode() string {
	sel := u.qualitySel.Selected
	for i, opt := range u.qualitySel.Options {
		if opt == sel && i < len(u.qualityItems) {
			q := u.qualityItems[i]
			if q.Height == 0 {
				return "audio"
			}
			return strconv.Itoa(q.Height)
		}
	}
	return ""
}

// ─── Download ─────────────────────────────────────────────────────────────────

func (u *UI) doDownload() {
	if u.videoInfo == nil {
		return
	}

	outDir := strings.TrimSpace(u.outdirEntry.Text)
	if outDir == "" {
		outDir = u.outDir
	}
	u.activeOutDir = outDir

	opts := DownloadOptions{
		URL:            strings.TrimSpace(u.urlEntry.Text),
		Quality:        u.selectedQualityCode(),
		Format:         strings.ToLower(u.formatSel.Selected),
		OutDir:         outDir,
		CookiesBrowser: u.cookiesBrowser(),
	}

	u.infoCard.Hide()
	u.optCard.Hide()
	u.progCard.SetTitle("Загрузка…")
	u.progCard.Show()
	u.progBar.SetValue(0)
	u.progPct.SetText("0%")
	u.progStatus.SetText("")
	u.speedLabel.SetText("")
	u.etaLabel.SetText("")
	u.sizeLabel.SetText("")
	u.cancelBtn.Show()
	u.openDirBtn.Hide()
	u.newDlBtn.Hide()
	u.lastFilePath = ""

	ctx, cancel := context.WithCancel(context.Background())
	u.cancelFn = cancel

	ch := make(chan Progress, 64)
	go StartDownload(ctx, opts, ch)
	go u.watchProgress(ch)
}

func (u *UI) watchProgress(ch <-chan Progress) {
	for p := range ch {
		u.applyProgress(p)
	}
}

func (u *UI) applyProgress(p Progress) {
	switch p.Type {
	case "info":
		// Промежуточные сообщения от yt-dlp на этапе извлечения метаданных
		// и постпроцессинга — обновляем только статус, не трогая прогресс-бар.
		if p.Message != "" {
			u.progStatus.SetText(p.Message)
		}

	case "progress":
		u.progBar.SetValue(p.Percent / 100)
		u.progPct.SetText(fmt.Sprintf("%.1f%%", p.Percent))
		if p.Size != "" {
			u.sizeLabel.SetText("📦 " + p.Size)
		}
		if p.Speed != "" {
			u.speedLabel.SetText("⚡ " + p.Speed)
		}
		if p.ETA != "" {
			u.etaLabel.SetText("⏳ " + p.ETA)
		}
		if p.Message != "" {
			u.progStatus.SetText(p.Message)
		}

	case "merge":
		u.progCard.SetTitle("Объединение…")
		u.progBar.SetValue(0.99)
		u.progPct.SetText("99%")
		u.progStatus.SetText(p.Message)
		u.speedLabel.SetText("")
		u.etaLabel.SetText("")

	case "done":
		u.progCard.SetTitle("Готово ✓")
		u.progBar.SetValue(1)
		u.progPct.SetText("100%")
		u.progStatus.SetText(p.Message)
		u.lastFilePath = p.FilePath
		u.cancelBtn.Hide()
		u.openDirBtn.Show()
		u.newDlBtn.Show()

	case "error":
		u.progCard.SetTitle("Ошибка")
		u.progBar.SetValue(0)
		u.progPct.SetText("")
		u.progStatus.SetText(p.Message)
		u.cancelBtn.Hide()
		u.newDlBtn.Show()
		dialog.ShowError(fmt.Errorf("%s", p.Message), u.win)
	}
}

// ─── Actions ──────────────────────────────────────────────────────────────────

func (u *UI) doCancel() {
	if u.cancelFn != nil {
		u.cancelFn()
		u.cancelFn = nil
	}
	u.progCard.SetTitle("Отменено")
	u.progStatus.SetText("")
	u.cancelBtn.Hide()
	u.newDlBtn.Show()
}

func (u *UI) doOpenDir() {
	dir := strings.TrimSpace(u.activeOutDir)
	if dir == "" {
		dir = u.outDir
	}
	if abs, err := filepath.Abs(dir); err == nil {
		dir = abs
	}
	dir = filepath.Clean(dir)

	switch runtime.GOOS {
	case "windows":
		// explorer.exe особенно придирчив: forward-слэши, относительные пути
		// или пустой аргумент — и он молча открывает Documents.
		// Если есть конкретный файл из последней загрузки, выделяем его в проводнике
		// через /select; иначе открываем саму папку. Игнорируем код возврата —
		// explorer возвращает 1 даже при успехе.
		if u.lastFilePath != "" {
			if absFile, err := filepath.Abs(u.lastFilePath); err == nil {
				absFile = filepath.Clean(absFile)
				if _, err := os.Stat(absFile); err == nil {
					exec.Command("explorer", "/select,"+absFile).Start() //nolint:errcheck
					return
				}
			}
		}
		exec.Command("explorer", dir).Start() //nolint:errcheck
	case "darwin":
		exec.Command("open", dir).Start() //nolint:errcheck
	default:
		for _, bin := range []string{"xdg-open", "nautilus", "dolphin", "thunar", "nemo"} {
			if path, err := exec.LookPath(bin); err == nil {
				exec.Command(path, dir).Start() //nolint:errcheck
				return
			}
		}
	}
}

func (u *UI) doNewDownload() {
	u.progCard.Hide()
	if u.isBatchMode {
		u.channelCard.Show()
		u.batchOptCard.Show()
	} else {
		u.infoCard.Show()
		u.optCard.Show()
	}
}

// ─── Batch download ───────────────────────────────────────────────────────────

func (u *UI) onBatchQualityChanged(s string) {
	if s == "Только аудио" {
		u.batchFormatSel.Options = []string{"MP3", "M4A"}
		if u.batchFormatSel.Selected != "MP3" && u.batchFormatSel.Selected != "M4A" {
			u.batchFormatSel.SetSelected("MP3")
		}
	} else {
		u.batchFormatSel.Options = []string{"MP4", "WebM", "MKV"}
		if u.batchFormatSel.Selected == "MP3" || u.batchFormatSel.Selected == "M4A" {
			u.batchFormatSel.SetSelected("MP4")
		}
	}
	u.batchFormatSel.Refresh()
}

func (u *UI) batchSelectedQuality() string {
	switch u.batchQualitySel.Selected {
	case "Лучшее":
		return "9999"
	case "Только аудио":
		return "audio"
	default:
		return strings.TrimSuffix(u.batchQualitySel.Selected, "p")
	}
}

func (u *UI) selectedEntries() []ChannelEntry {
	var result []ChannelEntry
	for i, ch := range u.channelChecks {
		if i < len(u.channelEntries) && ch.Checked {
			result = append(result, u.channelEntries[i])
		}
	}
	return result
}

func (u *UI) doBatchDownload() {
	entries := u.selectedEntries()
	if len(entries) == 0 {
		dialog.ShowInformation("Ничего не выбрано", "Отметьте хотя бы одно видео для скачивания.", u.win)
		return
	}

	outDir := strings.TrimSpace(u.batchOutdirEntry.Text)
	if outDir == "" {
		outDir = u.outDir
	}
	u.activeOutDir = outDir

	opts := DownloadOptions{
		Quality:        u.batchSelectedQuality(),
		Format:         strings.ToLower(u.batchFormatSel.Selected),
		OutDir:         outDir,
		CookiesBrowser: u.cookiesBrowser(),
	}

	u.batchFailedCount = 0
	u.batchFailedMsgs = nil

	u.channelCard.Hide()
	u.batchOptCard.Hide()
	u.progCard.SetTitle(fmt.Sprintf("Видео 1/%d…", len(entries)))
	u.progCard.Show()
	u.progBar.SetValue(0)
	u.progPct.SetText("0%")
	u.progStatus.SetText("")
	u.speedLabel.SetText("")
	u.etaLabel.SetText("")
	u.sizeLabel.SetText("")
	u.cancelBtn.Show()
	u.openDirBtn.Hide()
	u.newDlBtn.Hide()

	ctx, cancel := context.WithCancel(context.Background())
	u.cancelFn = cancel

	ch := make(chan BatchProgress, 64)
	go DownloadBatch(ctx, entries, opts, ch)
	go u.watchBatchProgress(ch)
}

func (u *UI) watchBatchProgress(ch <-chan BatchProgress) {
	for p := range ch {
		u.applyBatchProgress(p)
	}
}

func (u *UI) applyBatchProgress(p BatchProgress) {
	switch p.Type {
	case "start":
		title := fmt.Sprintf("Видео %d/%d", p.Index+1, p.Total)
		if u.batchFailedCount > 0 {
			title += fmt.Sprintf(" · ошибок: %d", u.batchFailedCount)
		}
		u.progCard.SetTitle(title)
		u.progStatus.SetText(p.VideoTitle)
		u.progBar.SetValue(0)
		u.progPct.SetText("0%")
		u.speedLabel.SetText("")
		u.etaLabel.SetText("")
		u.sizeLabel.SetText("")

	case "progress", "merge", "info":
		u.applyProgress(p.Inner)

	case "video_done":
		// следующий "start" сбросит UI на новое видео

	case "video_error":
		u.batchFailedCount++
		u.batchFailedMsgs = append(u.batchFailedMsgs,
			fmt.Sprintf("• %s\n  %s", p.VideoTitle, p.Inner.Message))

	case "batch_done":
		ok := p.Total - u.batchFailedCount
		title := fmt.Sprintf("Готово: %d из %d", ok, p.Total)
		if u.batchFailedCount > 0 {
			title += fmt.Sprintf(" (ошибок: %d)", u.batchFailedCount)
		}
		u.progCard.SetTitle(title)
		u.progBar.SetValue(1)
		u.progPct.SetText("100%")
		if u.batchFailedCount == 0 {
			u.progStatus.SetText("Все видео скачаны!")
		} else if ok == 0 {
			u.progStatus.SetText("Ни одно видео не скачалось.")
		} else {
			u.progStatus.SetText(fmt.Sprintf("Скачано %d, пропущено %d.", ok, u.batchFailedCount))
		}
		u.cancelBtn.Hide()
		u.openDirBtn.Show()
		u.newDlBtn.Show()

		if u.batchFailedCount > 0 {
			details := strings.Join(u.batchFailedMsgs, "\n\n")
			dialog.ShowError(fmt.Errorf("%s", details), u.win)
		}
	}
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

func formatDuration(sec int) string {
	if sec <= 0 {
		return ""
	}
	h := sec / 3600
	m := (sec % 3600) / 60
	s := sec % 60
	if h > 0 {
		return fmt.Sprintf("%d:%02d:%02d", h, m, s)
	}
	return fmt.Sprintf("%d:%02d", m, s)
}
