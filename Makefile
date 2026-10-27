EXE := $(if $(filter Windows_NT,$(OS)),.exe,)

.PHONY: build run test test-net clean

# Собирает оба бинарника и кладёт их в корень проекта, как раньше.
build:
	cargo build --release
	cp target/release/lader$(EXE) ./lader$(EXE)
	cp target/release/embed-thumbs$(EXE) ./embed-thumbs$(EXE)

run:
	cargo run --release --bin lader

test:
	cargo test

# Тесты, которые ходят в сеть: GitHub (установка yt-dlp) и превью YouTube.
test-net:
	cargo test -- --ignored

clean:
	cargo clean
	rm -f lader$(EXE) embed-thumbs$(EXE)
