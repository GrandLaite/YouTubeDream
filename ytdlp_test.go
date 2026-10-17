package main

import "testing"

func TestIsChannelURL(t *testing.T) {
	cases := []struct {
		url  string
		want bool
	}{
		// YouTube — одиночные видео
		{"https://www.youtube.com/watch?v=dQw4w9WgXcQ", false},
		{"https://youtu.be/dQw4w9WgXcQ", false},
		{"https://www.youtube.com/shorts/abc123", false},
		{"https://www.youtube.com/live/abc123", false},
		// видео, открытое из плейлиста, — всё ещё одно видео
		{"https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=PL123", false},

		// YouTube — списки
		{"https://www.youtube.com/playlist?list=PL123", true},
		{"https://www.youtube.com/@SomeChannel", true},
		{"https://www.youtube.com/@SomeChannel/videos", true},
		{"https://www.youtube.com/channel/UC123456", true},
		{"https://www.youtube.com/c/SomeChannel", true},
		{"https://www.youtube.com/user/SomeChannel", true},

		// X — одиночные твиты
		{"https://x.com/someuser/status/1234567890", false},
		{"https://twitter.com/someuser/status/1234567890", false},
		{"https://mobile.twitter.com/someuser/status/1234567890", false},
		{"https://x.com/someuser/status/1234567890?s=20&t=abc", false},
		{"https://x.com/i/status/1234567890", false},

		// X — профили. Режим канала для x.com не включается никогда:
		// ленты профиля yt-dlp не поддерживает, их ловит IsTwitterProfileURL.
		{"https://x.com/someuser", false},
		{"https://x.com/someuser/media", false},
		{"https://twitter.com/someuser/with_replies", false},
	}

	for _, c := range cases {
		if got := IsChannelURL(c.url); got != c.want {
			t.Errorf("IsChannelURL(%q) = %v, want %v", c.url, got, c.want)
		}
	}
}

func TestIsTwitterProfileURL(t *testing.T) {
	yes := []string{
		"https://x.com/someuser",
		"https://x.com/someuser/",
		"https://x.com/someuser/media",
		"https://twitter.com/someuser/with_replies",
		"https://x.com/someuser?lang=en",
	}
	no := []string{
		// конкретные твиты — это видео, а не профиль
		"https://x.com/someuser/status/1234567890",
		"https://twitter.com/someuser/status/1234567890?s=20",
		// не X вообще
		"https://www.youtube.com/@SomeChannel",
		"https://www.youtube.com/watch?v=abc",
	}
	for _, u := range yes {
		if !IsTwitterProfileURL(u) {
			t.Errorf("IsTwitterProfileURL(%q) = false, want true", u)
		}
	}
	for _, u := range no {
		if IsTwitterProfileURL(u) {
			t.Errorf("IsTwitterProfileURL(%q) = true, want false", u)
		}
	}
}

func TestIsTwitterURL(t *testing.T) {
	yes := []string{
		"https://x.com/u/status/1",
		"https://twitter.com/u/status/1",
		"https://www.x.com/u",
		"https://mobile.twitter.com/u",
	}
	no := []string{
		"https://www.youtube.com/watch?v=abc",
		"https://youtu.be/abc",
		"https://notx.com/u/status/1",
	}
	for _, u := range yes {
		if !IsTwitterURL(u) {
			t.Errorf("IsTwitterURL(%q) = false, want true", u)
		}
	}
	for _, u := range no {
		if IsTwitterURL(u) {
			t.Errorf("IsTwitterURL(%q) = true, want false", u)
		}
	}
}

func TestNeedsAuthError(t *testing.T) {
	yes := []string{
		"ERROR: [twitter] 123: NSFW tweet requires authentication",
		"Sign in to confirm your age",
		"This account is private",
		"HTTP Error 429: rate limit exceeded",
	}
	no := []string{
		"ERROR: unable to download video data: HTTP Error 404: Not Found",
		"ffmpeg not found",
	}
	for _, m := range yes {
		if !NeedsAuthError(m) {
			t.Errorf("NeedsAuthError(%q) = false, want true", m)
		}
	}
	for _, m := range no {
		if NeedsAuthError(m) {
			t.Errorf("NeedsAuthError(%q) = true, want false", m)
		}
	}
}
