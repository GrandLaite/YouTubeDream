BINARY := ytdl.exe
BUILD_FLAGS := -ldflags="-s -w -H windowsgui"

EMBED_BINARY := embed-thumbs.exe
EMBED_FLAGS := -ldflags="-s -w"

.PHONY: build run clean embed-thumbs

build:
	go build $(BUILD_FLAGS) -o $(BINARY) .

run: build
	./$(BINARY)

embed-thumbs:
	go build $(EMBED_FLAGS) -o $(EMBED_BINARY) ./tools/embed-thumbs

clean:
	rm -f $(BINARY) $(EMBED_BINARY)
