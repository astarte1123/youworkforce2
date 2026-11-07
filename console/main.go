// YouWorkforce console (Go). Server-rendered + static PWA, reverse-proxies /api
// to the Rust core so the browser stays same-origin (session cookies + SSE work).
package main

import (
	"log"
	"net/http"
	"net/http/httputil"
	"net/url"
	"os"
	"strings"
	"time"
)

func env(k, d string) string {
	if v := os.Getenv(k); v != "" {
		return v
	}
	return d
}

func main() {
	coreURL := env("YW_CORE_URL", "http://localhost:8080")
	bind := env("YW_CONSOLE_BIND", "0.0.0.0:8081")

	target, err := url.Parse(coreURL)
	if err != nil {
		log.Fatalf("bad core url: %v", err)
	}
	proxy := httputil.NewSingleHostReverseProxy(target)
	// SSE needs flushing + no response buffering
	proxy.FlushInterval = 100 * time.Millisecond

	mux := http.NewServeMux()

	// /api/* -> core (cookies and headers pass through unchanged)
	mux.HandleFunc("/api/", func(w http.ResponseWriter, r *http.Request) {
		proxy.ServeHTTP(w, r)
	})

	mux.HandleFunc("/health", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.Write([]byte(`{"ok":true,"service":"console"}`))
	})

	// static files (console pages + worker PWA)
	fs := http.FileServer(http.Dir("static"))
	mux.Handle("/", noCacheHTML(fs))

	log.Printf("console listening on %s -> core %s", bind, coreURL)
	srv := &http.Server{Addr: bind, Handler: mux}
	log.Fatal(srv.ListenAndServe())
}

// Avoid caching HTML during the pilot so UI changes show immediately.
func noCacheHTML(h http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if strings.HasSuffix(r.URL.Path, ".html") || r.URL.Path == "/" {
			w.Header().Set("Cache-Control", "no-store")
		}
		h.ServeHTTP(w, r)
	})
}
