BINARY := ytdl
BUILD_FLAGS := -ldflags="-s -w"

.PHONY: build run clean

build:
	go build $(BUILD_FLAGS) -o $(BINARY) .

run: build
	./$(BINARY)

clean:
	rm -f $(BINARY)
