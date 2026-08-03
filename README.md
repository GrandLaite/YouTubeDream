# YT Downloader

GUI-приложение на Go + Fyne для скачивания видео с YouTube.

Поддерживает:
- одиночные видео по ссылке
- каналы и плейлисты (список с чекбоксами, постраничная загрузка)

Зависимости: [yt-dlp](https://github.com/yt-dlp/yt-dlp), [ffmpeg](https://ffmpeg.org/)

## Сборка под Windows 11

```
go build -ldflags="-s -w -H windowsgui" -o ytdl.exe .
```

Или через Make:

```
make build
```

Флаг `-H windowsgui` убирает консольное окно при запуске.
