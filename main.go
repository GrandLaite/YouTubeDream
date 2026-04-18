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
	"strconv"
	"strings"
	"time"

	"fyne.io/fyne/v2"
	"fyne.io/fyne/v2/app"
	"fyne.io/fyne/v2/canvas"
	"fyne.io/fyne/v2/container"
	"fyne.io/fyne/v2/dialog"
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
	urlEntry  *widget.Entry
	fetchBtn  *widget.Button
	fetchProg *widget.ProgressBarInfinite

	// Info
	thumbImg      *canvas.Image
	titleLabel    *widget.Label
	uploaderLabel *widget.Label
	durationLabel *widget.Label
	infoCard      *widget.Card

	// Options
	qualitySel  *widget.Select
	formatSel   *widget.Select
	hintLabel   *widget.Label
	outdirEntry *widget.Entry
	downloadBtn *widget.Button
	optCard     *widget.Card

	// Progress
	progBar    *widget.ProgressBar
	progStatus *widget.Label // "загрузка / объединение / готово / ошибка"
	progPct    *widget.Label
	speedLabel *widget.Label
	etaLabel   *widget.Label
	sizeLabel  *widget.Label
	cancelBtn  *widget.Button
	openDirBtn *widget.Button
	newDlBtn   *widget.Button
	progCard   *widget.Card

	// State
	videoInfo    *VideoInfo
	qualityItems []Quality
	cancelFn     context.CancelFunc
	outDir       string
	lastFilePath string
}

func newUI(a fyne.App, w fyne.Window) *UI {
	home, _ := os.UserHomeDir()
	return &UI{
		app:    a,
		win:    w,
		outDir: filepath.Join(home, "Downloads"),
	}
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
	urlCard := widget.NewCard("", "", container.NewVBox(urlRow, u.fetchProg))

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
		dialog.ShowFolderOpen(func(uri fyne.ListableURI, err error) {
			if err != nil || uri == nil {
				return
			}
			u.outdirEntry.SetText(uri.Path())
		}, u.win)
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
	body := container.NewVBox(urlCard, u.infoCard, u.optCard, u.progCard)
	scroll := container.NewScroll(body)

	return container.NewPadded(scroll)
}

// ─── Fetch ────────────────────────────────────────────────────────────────────

func (u *UI) doFetch() {
	url := strings.TrimSpace(u.urlEntry.Text)
	if url == "" {
		dialog.ShowError(fmt.Errorf("введите ссылку"), u.win)
		return
	}

	u.fetchBtn.Disable()
	u.fetchProg.Show()
	u.infoCard.Hide()
	u.optCard.Hide()
	u.progCard.Hide()

	go func() {
		ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
		defer cancel()

		info, err := GetVideoInfo(ctx, url)

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

	opts := DownloadOptions{
		URL:     strings.TrimSpace(u.urlEntry.Text),
		Quality: u.selectedQualityCode(),
		Format:  strings.ToLower(u.formatSel.Selected),
		OutDir:  outDir,
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
	dir := strings.TrimSpace(u.outdirEntry.Text)
	if dir == "" {
		dir = u.outDir
	}
	for _, bin := range []string{"xdg-open", "nautilus", "dolphin", "thunar", "nemo"} {
		if path, err := exec.LookPath(bin); err == nil {
			exec.Command(path, dir).Start() //nolint:errcheck
			return
		}
	}
}

func (u *UI) doNewDownload() {
	u.progCard.Hide()
	u.infoCard.Show()
	u.optCard.Show()
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
