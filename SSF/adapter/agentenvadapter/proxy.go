package agentenvadapter

import (
	"errors"
	"net/http"
	"net/http/httputil"
	"net/url"
	"strings"
)

// Only envd process/filesystem services are forwarded. Lifecycle and native
// management APIs never pass through this route. The target instance is fixed
// by the persisted operation, not by caller-supplied routing headers.
func guestPath(p string) bool {
	if strings.ContainsAny(p, "%?#\\") || strings.Contains(p, "..") {
		return false
	}
	parts := strings.Split(strings.TrimPrefix(p, "/"), "/")
	return len(parts) == 2 && (parts[0] == "process.Process" || parts[0] == "filesystem.Filesystem") && safeID.MatchString(parts[1])
}
func forwardGuest(w http.ResponseWriter, r *http.Request, client *HTTPClient, target, path, sandbox, token string) {
	u, e := url.Parse(target)
	if e != nil {
		problem(w, 502, e)
		return
	}
	proxy := &httputil.ReverseProxy{Transport: client.Client.Transport, FlushInterval: -1, Rewrite: func(pr *httputil.ProxyRequest) {
		pr.SetURL(u)
		pr.Out.URL.Path = path
		pr.Out.URL.RawPath = ""
		pr.Out.URL.RawQuery = ""
		pr.Out.Host = u.Host
		for k := range pr.Out.Header {
			lower := strings.ToLower(k)
			if strings.HasPrefix(lower, "x-agentenv-") || lower == "x-api-key" || lower == "x-access-token" || strings.HasPrefix(lower, "x-forwarded-") {
				pr.Out.Header.Del(k)
			}
		}
		if sandbox != "" {
			pr.Out.Header.Set("x-agentenv-sandbox-id", sandbox)
			pr.Out.Header.Set("x-agentenv-target-port", "49983")
			pr.Out.Header.Set("X-API-Key", client.APIKey)
			if token != "" {
				pr.Out.Header.Set("X-Access-Token", token)
			}
		}
	}, ErrorHandler: func(w http.ResponseWriter, _ *http.Request, e error) { problem(w, 502, e) }}
	proxy.ServeHTTP(w, r)
}
func (c *Control) guestHTTP(w http.ResponseWriter, r *http.Request, p Principal, parts []string) {
	suffix := "/" + strings.Join(parts[4:], "/")
	if r.Method != "POST" || !guestPath(suffix) {
		problem(w, 400, ErrUnsupported)
		return
	}
	o, e := c.Store.Get(r.Context(), parts[2])
	if e != nil || o.Tenant != p.Tenant {
		problem(w, 404, ErrNotFound)
		return
	}
	if o.State != "Ready" || o.Stop {
		problem(w, 409, errors.New("sandbox not ready"))
		return
	}
	for _, worker := range c.Config.Workers {
		if worker.Name == o.Result.Placement.Node {
			forwardGuest(w, r, c.HTTP, worker.URL, "/v1/operations/"+o.ID+"/guest"+suffix, "", "")
			return
		}
	}
	problem(w, 503, errors.New("pinned worker unavailable"))
}
func (n *Node) guestHTTP(w http.ResponseWriter, r *http.Request, x NodeRecord, parts []string) {
	suffix := "/" + strings.Join(parts[4:], "/")
	if r.Method != "POST" || !guestPath(suffix) {
		problem(w, 400, ErrUnsupported)
		return
	}
	if x.State != "Ready" || x.Stop {
		problem(w, 409, errors.New("runtime not ready"))
		return
	}
	if e := n.verifyRuntime(x, "ready"); e != nil {
		problem(w, 409, e)
		return
	}
	var native struct {
		Token string `json:"envdAccessToken"`
	}
	if e := n.Agent.Call(r.Context(), "GET", base(n.Config.AgentURL)+"/sandboxes/"+x.Runtime.SandboxID, nil, &native); e != nil {
		problem(w, 502, e)
		return
	}
	forwardGuest(w, r, n.Agent, n.Config.AgentURL, suffix, x.Runtime.SandboxID, native.Token)
}
