// Google notification ingress for the existing private personal-operations inbox.
package main

import (
	"bytes"
	"context"
	"crypto/rand"
	"crypto/subtle"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"net/http"
	"net/http/httputil"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"time"
)

type Config struct {
	EnableRenewal                                                                                                   bool
	Root, Account, Topic, Subscription, PublicURL, Listen, Upstream, Secret, CalendarToken, Gog, OpsURL, OpsKeyFile string
}
type Channel struct {
	ID         string `json:"id"`
	ResourceID string `json:"resourceId"`
	Expiration string `json:"expiration"`
}
type State struct {
	Channels        []Channel `json:"channels"`
	GmailExpiration int64     `json:"gmail_expiration_ms"`
	LastRenew       int64     `json:"last_renew_ms"`
	LastGmail       int64     `json:"last_gmail_notification_ms"`
	LastCalendar    int64     `json:"last_calendar_notification_ms"`
	GmailError      string    `json:"gmail_error,omitempty"`
	CalendarError   string    `json:"calendar_error,omitempty"`
}
type Bridge struct {
	cfg    Config
	mu     sync.Mutex
	state  State
	client *http.Client
	slots  chan struct{}
}

func randomID() string {
	b := make([]byte, 32)
	if _, err := rand.Read(b); err != nil {
		panic(err)
	}
	return hex.EncodeToString(b)
}
func atomicJSON(path string, v any) error {
	b, e := json.MarshalIndent(v, "", "  ")
	if e != nil {
		return e
	}
	f, e := os.CreateTemp(filepath.Dir(path), ".state-*")
	if e != nil {
		return e
	}
	defer os.Remove(f.Name())
	if e = f.Chmod(0600); e == nil {
		_, e = f.Write(b)
	}
	if e == nil {
		e = f.Sync()
	}
	ce := f.Close()
	if e != nil {
		return e
	}
	if ce != nil {
		return ce
	}
	return os.Rename(f.Name(), path)
}
func (b *Bridge) saveLocked() error {
	return atomicJSON(filepath.Join(b.cfg.Root, "state.json"), b.state)
}
func equal(a, c string) bool {
	return len(c) >= 32 && subtle.ConstantTimeCompare([]byte(a), []byte(c)) == 1
}
func (b *Bridge) ingest(ctx context.Context, id, source, kind string, payload any) error {
	key, e := os.ReadFile(b.cfg.OpsKeyFile)
	if e != nil {
		return e
	}
	body, e := json.Marshal(map[string]any{"event_id": id, "source": source, "kind": kind, "payload": payload})
	if e != nil {
		return e
	}
	req, e := http.NewRequestWithContext(ctx, "POST", b.cfg.OpsURL, bytes.NewReader(body))
	if e != nil {
		return e
	}
	req.Header.Set("Authorization", "Bearer "+strings.TrimSpace(string(key)))
	req.Header.Set("Content-Type", "application/json")
	res, e := b.client.Do(req)
	if e != nil {
		return errors.New("private event inbox unavailable")
	}
	defer res.Body.Close()
	if res.StatusCode != 200 {
		return fmt.Errorf("event inbox status %d", res.StatusCode)
	}
	var receipt struct {
		Accepted bool `json:"accepted"`
	}
	if e = json.NewDecoder(io.LimitReader(res.Body, 8192)).Decode(&receipt); e != nil {
		return e
	}
	if !receipt.Accepted {
		return errors.New("event not accepted")
	}
	return nil
}
func (b *Bridge) notifications(w http.ResponseWriter, r *http.Request) {
	if r.Method != "POST" {
		w.WriteHeader(405)
		return
	}
	source := ""
	id := ""
	var payload any
	switch r.URL.Path {
	case "/google-notifications/gmail":
		if !equal(r.URL.Query().Get("token"), b.cfg.Secret) {
			w.WriteHeader(401)
			return
		}
		var env struct {
			Subscription string `json:"subscription"`
			Message      struct {
				Data string `json:"data"`
				ID   string `json:"messageId"`
			} `json:"message"`
		}
		raw, e := io.ReadAll(http.MaxBytesReader(w, r.Body, 16384))
		if e != nil {
			w.WriteHeader(413)
			return
		}
		if json.Unmarshal(raw, &env) != nil || env.Subscription != b.cfg.Subscription || len(env.Message.ID) == 0 || len(env.Message.ID) > 128 {
			w.WriteHeader(400)
			return
		}
		decoded, e := base64.StdEncoding.DecodeString(env.Message.Data)
		if e != nil {
			w.WriteHeader(400)
			return
		}
		var note struct {
			Email   string      `json:"emailAddress"`
			History json.Number `json:"historyId"`
		}
		if json.Unmarshal(decoded, &note) != nil || note.Email != b.cfg.Account {
			w.WriteHeader(400)
			return
		}
		if _, e = note.History.Int64(); e != nil {
			w.WriteHeader(400)
			return
		}
		source = "gmail"
		id = "google-push:" + env.Message.ID
		payload = map[string]any{"history_id": note.History.String()}
	case "/google-notifications/calendar":
		if !equal(r.Header.Get("X-Goog-Channel-Token"), b.cfg.CalendarToken) {
			w.WriteHeader(401)
			return
		}
		cid := r.Header.Get("X-Goog-Channel-ID")
		resource := r.Header.Get("X-Goog-Resource-ID")
		kind := r.Header.Get("X-Goog-Resource-State")
		num := r.Header.Get("X-Goog-Message-Number")
		if len(num) == 0 || len(num) > 64 || len(resource) == 0 || len(resource) > 512 || (kind != "sync" && kind != "exists" && kind != "not_exists") {
			w.WriteHeader(400)
			return
		}
		valid := false
		b.mu.Lock()
		for _, c := range b.state.Channels {
			if c.ID == cid && (c.ResourceID == "" || c.ResourceID == resource) {
				valid = true
			}
		}
		b.mu.Unlock()
		if !valid {
			w.WriteHeader(403)
			return
		}
		source = "calendar"
		id = "google-push:" + cid + ":" + num
		payload = map[string]any{"calendar_id": "primary", "resource_state": kind}
	default:
		w.WriteHeader(404)
		return
	}
	select {
	case b.slots <- struct{}{}:
		defer func() { <-b.slots }()
	default:
		w.WriteHeader(503)
		return
	}
	ctx, cancel := context.WithTimeout(r.Context(), 15*time.Second)
	defer cancel()
	kind := "email_changed"
	if source == "calendar" {
		kind = "calendar_changed"
	}
	if e := b.ingest(ctx, id, source, kind, payload); e != nil {
		log.Printf("%s notification could not reach private inbox", source)
		w.WriteHeader(503)
		return
	}
	b.mu.Lock()
	if source == "gmail" {
		b.state.LastGmail = time.Now().UnixMilli()
	} else {
		b.state.LastCalendar = time.Now().UnixMilli()
	}
	e := b.saveLocked()
	b.mu.Unlock()
	if e != nil {
		log.Print("notification status persistence failed")
	}
	w.WriteHeader(204)
}
func (b *Bridge) handler() (http.Handler, error) {
	upstream, e := url.Parse(b.cfg.Upstream)
	if e != nil {
		return nil, e
	}
	if upstream.Host != "127.0.0.1:3335" && !strings.HasPrefix(upstream.Host, "127.0.0.1:") {
		return nil, errors.New("upstream must be loopback")
	}
	proxy := httputil.NewSingleHostReverseProxy(upstream)
	proxy.ErrorLog = log.New(io.Discard, "", 0)
	proxy.ErrorHandler = func(w http.ResponseWriter, r *http.Request, e error) { w.WriteHeader(502) }
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/google-notifications/status" {
			if !equal(strings.TrimPrefix(r.Header.Get("Authorization"), "Bearer "), b.cfg.Secret) {
				w.WriteHeader(401)
				return
			}
			b.mu.Lock()
			defer b.mu.Unlock()
			w.Header().Set("Content-Type", "application/json")
			json.NewEncoder(w).Encode(b.state)
			return
		}
		if strings.HasPrefix(r.URL.Path, "/google-notifications/") {
			b.notifications(w, r)
			return
		}
		proxy.ServeHTTP(w, r)
	}), nil
}
func (b *Bridge) gog(args ...string) ([]byte, error) {
	ctx, cancel := context.WithTimeout(context.Background(), 75*time.Second)
	defer cancel()
	all := append([]string{"--account", b.cfg.Account, "--json", "--no-input"}, args...)
	cmd := exec.CommandContext(ctx, b.cfg.Gog, all...)
	var out bytes.Buffer
	cmd.Stdout = &out
	cmd.Stderr = io.Discard
	if e := cmd.Run(); e != nil {
		return nil, errors.New("Google API call failed; existing permissions or connectivity need checking")
	}
	return out.Bytes(), nil
}
func (b *Bridge) api(api, method string, params, body any) ([]byte, error) {
	f, e := os.CreateTemp(b.cfg.Root, ".request-*")
	if e != nil {
		return nil, e
	}
	defer os.Remove(f.Name())
	if e = f.Chmod(0600); e != nil {
		f.Close()
		return nil, e
	}
	if e = json.NewEncoder(f).Encode(body); e != nil {
		f.Close()
		return nil, e
	}
	if e = f.Close(); e != nil {
		return nil, e
	}
	p, e := json.Marshal(params)
	if e != nil {
		return nil, e
	}
	version := "v3"
	scope := "https://www.googleapis.com/auth/calendar"
	if api == "pubsub" {
		version = "v1"
		scope = "https://www.googleapis.com/auth/pubsub"
	}
	return b.gog("api", "call", api, version, method, "--params", string(p), "--body", "@"+f.Name(), "--scope", scope, "--allow-write", "--force")
}
func (b *Bridge) configurePush() error {
	_, e := b.api("pubsub", "projects.subscriptions.modifyPushConfig", map[string]string{"subscription": b.cfg.Subscription}, map[string]any{"pushConfig": map[string]string{"pushEndpoint": b.cfg.PublicURL + "/google-notifications/gmail?token=" + b.cfg.Secret}})
	return e
}
func (b *Bridge) renew() {
	raw, ge := b.gog("gmail", "watch", "start", "--topic", b.cfg.Topic, "--label", "INBOX")
	var g struct {
		Watch struct {
			Expiration int64 `json:"providerExpirationMs"`
		} `json:"watch"`
	}
	if ge == nil {
		ge = json.Unmarshal(raw, &g)
		if ge == nil && g.Watch.Expiration <= time.Now().UnixMilli() {
			ge = errors.New("missing Gmail watch expiration")
		}
	}
	b.mu.Lock()
	if ge == nil {
		b.state.GmailExpiration = g.Watch.Expiration
		b.state.GmailError = ""
	} else {
		b.state.GmailError = ge.Error()
	}
	e := b.saveLocked()
	b.mu.Unlock()
	if e != nil {
		log.Print("watch state persistence failed")
		return
	}
	pending := Channel{ID: randomID(), Expiration: fmt.Sprint(time.Now().Add(7 * 24 * time.Hour).UnixMilli())}
	b.mu.Lock()
	old := append([]Channel(nil), b.state.Channels...)
	b.state.Channels = append(b.state.Channels, pending)
	e = b.saveLocked()
	b.mu.Unlock()
	if e != nil {
		log.Print("cannot persist pending calendar channel")
		return
	}
	raw, ce := b.api("calendar", "events.watch", map[string]string{"calendarId": "primary"}, map[string]any{"id": pending.ID, "type": "web_hook", "address": b.cfg.PublicURL + "/google-notifications/calendar", "token": b.cfg.CalendarToken, "expiration": pending.Expiration})
	var c Channel
	if ce == nil {
		ce = json.Unmarshal(raw, &c)
		if ce == nil && (c.ID != pending.ID || c.ResourceID == "" || c.Expiration == "") {
			ce = errors.New("invalid Calendar watch receipt")
		}
	}
	b.mu.Lock()
	if ce == nil {
		for i := range b.state.Channels {
			if b.state.Channels[i].ID == pending.ID {
				b.state.Channels[i] = c
			}
		}
		b.state.CalendarError = ""
	} else {
		b.state.CalendarError = ce.Error()
	}
	if ce == nil && ge == nil {
		b.state.LastRenew = time.Now().UnixMilli()
	}
	e = b.saveLocked()
	b.mu.Unlock()
	if e != nil {
		log.Print("watch receipt persistence failed")
		return
	}
	if ce == nil {
		for _, previous := range old {
			if previous.ResourceID == "" {
				continue
			}
			if _, stopErr := b.api("calendar", "channels.stop", map[string]string{}, map[string]string{"id": previous.ID, "resourceId": previous.ResourceID}); stopErr == nil {
				b.mu.Lock()
				kept := b.state.Channels[:0]
				for _, v := range b.state.Channels {
					if v.ID != previous.ID {
						kept = append(kept, v)
					}
				}
				b.state.Channels = kept
				e = b.saveLocked()
				b.mu.Unlock()
				if e != nil {
					log.Print("channel retirement persistence failed")
				}
			}
		}
	}
	if ge != nil || ce != nil {
		log.Print("Google watch renewal incomplete; will retry")
	} else {
		log.Print("Gmail and Calendar watches renewed")
	}
}
func main() {
	if len(os.Args) != 3 {
		log.Fatal("usage: zeroclaw-google-push serve|configure-push|renew CONFIG.json")
	}
	raw, e := os.ReadFile(os.Args[2])
	if e != nil {
		log.Fatal("cannot read bridge config")
	}
	var c Config
	if json.Unmarshal(raw, &c) != nil || len(c.Secret) < 32 || len(c.CalendarToken) < 32 || c.Account == "" {
		log.Fatal("invalid bridge config")
	}
	b := &Bridge{cfg: c, client: &http.Client{Timeout: 15 * time.Second}, slots: make(chan struct{}, 8)}
	raw, e = os.ReadFile(filepath.Join(c.Root, "state.json"))
	if e == nil {
		if json.Unmarshal(raw, &b.state) != nil {
			log.Fatal("invalid watch state")
		}
	} else if !os.IsNotExist(e) {
		log.Fatal("cannot load watch state")
	}
	switch os.Args[1] {
	case "configure-push":
		if b.configurePush() != nil {
			log.Fatal("Pub/Sub push configuration failed")
		}
		log.Print("Pub/Sub push configured")
		return
	case "renew":
		b.renew()
		return
	case "serve":
	default:
		log.Fatal("unknown command")
	}
	h, e := b.handler()
	if e != nil {
		log.Fatal(e)
	}
	go func() {
		for {
			b.mu.Lock()
			due := time.Now().UnixMilli()-b.state.LastRenew >= int64((20*time.Hour)/time.Millisecond)
			b.mu.Unlock()
			if due && b.cfg.EnableRenewal {
				b.renew()
			}
			time.Sleep(5 * time.Minute)
		}
	}()
	server := &http.Server{Addr: c.Listen, Handler: h, ReadHeaderTimeout: 10 * time.Second, IdleTimeout: 90 * time.Second, MaxHeaderBytes: 32768}
	log.Print("Google notification bridge ready")
	log.Fatal(server.ListenAndServe())
}
