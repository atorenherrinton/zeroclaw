package main

import (
	"bufio"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

func fixture(t *testing.T) (*Bridge, *atomic.Int32) {
	t.Helper()
	dir := t.TempDir()
	key := filepath.Join(dir, "key")
	if err := os.WriteFile(key, []byte("private-inbox-key"), 0600); err != nil {
		t.Fatal(err)
	}
	n := &atomic.Int32{}
	inbox := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("Authorization") != "Bearer private-inbox-key" {
			t.Error("missing private auth")
			w.WriteHeader(401)
			return
		}
		var event map[string]any
		if json.NewDecoder(r.Body).Decode(&event) != nil {
			t.Error("invalid event")
		}
		if event["kind"] != "email_changed" && event["kind"] != "calendar_changed" {
			t.Error("unexpected event kind")
		}
		n.Add(1)
		fmt.Fprint(w, `{"accepted":true}`)
	}))
	t.Cleanup(inbox.Close)
	b := &Bridge{cfg: Config{Root: dir, Account: "owner@example.com", Subscription: "projects/example/subscriptions/gmail", Secret: strings.Repeat("s", 64), CalendarToken: strings.Repeat("c", 64), OpsKeyFile: key, OpsURL: inbox.URL}, client: &http.Client{Timeout: time.Second}, slots: make(chan struct{}, 8)}
	b.state.Channels = []Channel{{ID: "known", ResourceID: "resource"}}
	return b, n
}
func gmailBody(email, subscription string) string {
	data := base64.StdEncoding.EncodeToString([]byte(fmt.Sprintf(`{"emailAddress":%q,"historyId":"123"}`, email)))
	body, _ := json.Marshal(map[string]any{"subscription": subscription, "message": map[string]string{"messageId": "42", "data": data}})
	return string(body)
}
func TestGmailAuthenticatedBoundedIngress(t *testing.T) {
	b, n := fixture(t)
	valid := gmailBody(b.cfg.Account, b.cfg.Subscription)
	cases := []struct {
		name, path, body string
		want             int
	}{{"valid", "?token=" + b.cfg.Secret, valid, 204}, {"missing auth", "", valid, 401}, {"wrong auth", "?token=wrong", valid, 401}, {"wrong mailbox", "?token=" + b.cfg.Secret, gmailBody("other@example.com", b.cfg.Subscription), 400}, {"wrong subscription", "?token=" + b.cfg.Secret, gmailBody(b.cfg.Account, "other"), 400}, {"oversized", "?token=" + b.cfg.Secret, strings.Repeat("x", 20000), 413}}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			r := httptest.NewRequest("POST", "http://localhost/google-notifications/gmail"+c.path, strings.NewReader(c.body))
			w := httptest.NewRecorder()
			b.notifications(w, r)
			if w.Code != c.want {
				t.Fatalf("got %d want %d", w.Code, c.want)
			}
		})
	}
	if n.Load() != 1 {
		t.Fatal("untrusted requests reached inbox")
	}
	if b.state.LastGmail == 0 {
		t.Fatal("receipt not recorded")
	}
}
func TestCalendarChannelBinding(t *testing.T) {
	b, n := fixture(t)
	for _, c := range []struct {
		id, resource, token string
		want                int
	}{{"known", "resource", b.cfg.CalendarToken, 204}, {"unknown", "resource", b.cfg.CalendarToken, 403}, {"known", "other", b.cfg.CalendarToken, 403}, {"known", "resource", "wrong", 401}} {
		r := httptest.NewRequest("POST", "http://localhost/google-notifications/calendar", nil)
		r.Header.Set("X-Goog-Channel-Token", c.token)
		r.Header.Set("X-Goog-Channel-ID", c.id)
		r.Header.Set("X-Goog-Resource-ID", c.resource)
		r.Header.Set("X-Goog-Resource-State", "sync")
		r.Header.Set("X-Goog-Message-Number", "1")
		w := httptest.NewRecorder()
		b.notifications(w, r)
		if w.Code != c.want {
			t.Fatalf("got %d want %d", w.Code, c.want)
		}
	}
	if n.Load() != 1 {
		t.Fatal("invalid channel reached inbox")
	}
}
func TestInboxFailureRemainsRetryable(t *testing.T) {
	b, _ := fixture(t)
	b.cfg.OpsURL = "http://127.0.0.1:1/events"
	r := httptest.NewRequest("POST", "http://localhost/google-notifications/gmail?token="+b.cfg.Secret, strings.NewReader(gmailBody(b.cfg.Account, b.cfg.Subscription)))
	w := httptest.NewRecorder()
	b.notifications(w, r)
	if w.Code != 503 || b.state.LastGmail != 0 {
		t.Fatal("failed inbox write acknowledged")
	}
}
func TestExistingPhoneRequestsAreProxied(t *testing.T) {
	b, _ := fixture(t)
	up := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.RequestURI() != "/phone/status?a=1" || r.Header.Get("X-Twilio-Signature") != "signature" {
			t.Error("phone request changed")
		}
		w.Header().Set("X-Upstream", "phone")
		fmt.Fprint(w, "unchanged")
	}))
	defer up.Close()
	b.cfg.Upstream = up.URL
	h, e := b.handler()
	if e != nil {
		t.Fatal(e)
	}
	w := httptest.NewRecorder()
	r := httptest.NewRequest("POST", "http://public.example/phone/status?a=1", strings.NewReader("body"))
	r.Header.Set("X-Twilio-Signature", "signature")
	h.ServeHTTP(w, r)
	if w.Code != 200 || w.Body.String() != "unchanged" || w.Header().Get("X-Upstream") != "phone" {
		t.Fatal("proxy response changed")
	}
}
func TestUnknownNotificationPathsStayClosed(t *testing.T) {
	b, _ := fixture(t)
	b.cfg.Upstream = "http://127.0.0.1:1"
	h, e := b.handler()
	if e != nil {
		t.Fatal(e)
	}
	w := httptest.NewRecorder()
	h.ServeHTTP(w, httptest.NewRequest("POST", "http://localhost/google-notifications/other", nil))
	if w.Code != 404 {
		t.Fatal(w.Code)
	}
}

func TestPhoneWebSocketUpgradeSurvivesProxy(t *testing.T) {
	b, _ := fixture(t)
	up := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		conn, buf, err := w.(http.Hijacker).Hijack()
		if err != nil {
			t.Error(err)
			return
		}
		defer conn.Close()
		fmt.Fprint(buf, "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n")
		buf.Flush()
		input := make([]byte, 4)
		if _, err = io.ReadFull(buf, input); err != nil {
			return
		}
		if string(input) != "ping" {
			t.Error("stream changed")
		}
		conn.Write([]byte("pong"))
	}))
	defer up.Close()
	b.cfg.Upstream = up.URL
	h, err := b.handler()
	if err != nil {
		t.Fatal(err)
	}
	proxy := httptest.NewServer(h)
	defer proxy.Close()
	u, _ := url.Parse(proxy.URL)
	conn, err := net.DialTimeout("tcp", u.Host, time.Second)
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close()
	conn.SetDeadline(time.Now().Add(3 * time.Second))
	fmt.Fprintf(conn, "GET /voice/media/test HTTP/1.1\r\nHost: %s\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n", u.Host)
	reader := bufio.NewReader(conn)
	res, err := http.ReadResponse(reader, nil)
	if err != nil {
		t.Fatal(err)
	}
	if res.StatusCode != 101 {
		t.Fatal(res.StatusCode)
	}
	conn.Write([]byte("ping"))
	reply := make([]byte, 4)
	if _, err = io.ReadFull(reader, reply); err != nil {
		t.Fatal(err)
	}
	if string(reply) != "pong" {
		t.Fatal("upgrade stream failed")
	}
}
