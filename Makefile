EXE := $(if $(filter Windows_NT,$(OS)),.exe,)

.PHONY: build run test test-net clean host extension extension-external

# Собирает все бинарники и кладёт их в корень проекта, как раньше.
build:
	cargo build --release
	cp target/release/lader$(EXE) ./lader$(EXE)
	cp target/release/embed-thumbs$(EXE) ./embed-thumbs$(EXE)
	cp target/release/lader-host$(EXE) ./lader-host$(EXE)

# Помощник браузерного расширения. Запуск без аргументов регистрирует его в Edge.
host:
	cargo build --release --bin lader-host
	cp target/release/lader-host$(EXE) ./lader-host$(EXE)

# Архив расширения для Edge (папка extension).
extension:
	rm -f lader-extension.zip
	cd extension && zip -r ../lader-extension.zip . -x "*.DS_Store"

# CRX, updates.xml и NSIS-установщик с помощником. Ключ задаётся путём к PEM-файлу.
extension-external: host
	node packaging/build-extension.mjs --key "$(LADER_EXTENSION_KEY)"

run:
	cargo run --release --bin lader

test:
	cargo test

# Тесты, которые ходят в сеть: GitHub (установка yt-dlp) и превью YouTube.
test-net:
	cargo test -- --ignored

clean:
	cargo clean
	rm -f lader$(EXE) embed-thumbs$(EXE) lader-host$(EXE) lader-extension.zip
