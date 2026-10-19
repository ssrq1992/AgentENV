package agentenvadapter

import (
	"encoding/json"
	"errors"
	"net/url"
	"os"
	"sort"
	"ssf.dev/ssf/config"
	"strings"
)

type Worker struct {
	Name      string    `json:"name"`
	URL       string    `json:"url"`
	Resources Resources `json:"capacity"`
}
type Template struct {
	ID        string    `json:"id"` // immutable snapshot ID, not a moving alias
	Resources Resources `json:"resources"`
	RWSlots   int       `json:"rw_slots"`
	ROSlots   int       `json:"ro_slots"`
}
type Config struct {
	Listen        string               `json:"listen"`
	HealthListen  string               `json:"health_listen"`
	HookListen    string               `json:"hook_listen"`
	Cert          string               `json:"cert"`
	Key           string               `json:"key"`
	CA            string               `json:"ca"`
	Principals    map[string]Principal `json:"principals"`
	ControlPeers  []string             `json:"control_peers"`
	EvidencePeers []string             `json:"evidence_peers"`
	DatabaseURL   string               `json:"database_url"`
	Kubeconfig    string               `json:"kubeconfig"`
	SigningKey    string               `json:"signing_key"`
	PlanPublicKey string               `json:"plan_public_key"`
	Drivers       []config.Driver      `json:"drivers"`
	Workers       []Worker             `json:"workers"`
	Templates     map[string]Template  `json:"templates"`
	NodeName      string               `json:"node_name"`
	NodeUID       string               `json:"node_uid"`
	SSFEndpoint   string               `json:"ssf_endpoint"`
	AgentURL      string               `json:"agent_url"`
	AgentKeyFile  string               `json:"agent_key_file"`
	Root          string               `json:"root"`
	DeviceRoot    string               `json:"device_root"`
	RuntimeRoot   string               `json:"runtime_root"`
}

func Load(path string) (Config, error) {
	var c Config
	b, e := os.ReadFile(path)
	if e != nil {
		return c, e
	}
	e = json.Unmarshal(b, &c)
	if c.DatabaseURL == "" {
		c.DatabaseURL = os.Getenv("SSF_DATABASE_URL")
	}
	if c.NodeName == "" {
		c.NodeName = os.Getenv("NODE_NAME")
	}
	if c.Root == "" {
		c.Root = "/var/lib/agentenvadapter"
	}
	if c.DeviceRoot == "" {
		c.DeviceRoot = "/var/lib/ssf"
	}
	if c.RuntimeRoot == "" {
		c.RuntimeRoot = "/var/lib/agentenv-external"
	}
	if c.Listen == "" {
		c.Listen = ":9444"
	}
	if c.HookListen == "" {
		c.HookListen = "127.0.0.1:9445"
	}
	return c, e
}
func (c Config) Validate(role string) error {
	if c.Cert == "" || c.Key == "" || c.CA == "" {
		return errors.New("TLS certificate, key and CA required")
	}
	if role == "control" {
		if len(c.Principals) == 0 || len(c.Workers) == 0 || c.DatabaseURL == "" {
			return errors.New("principals, dedicated workers and PostgreSQL required")
		}
		tenants := map[string]string{}
		for peer, p := range c.Principals {
			u, err := url.Parse(peer)
			if err != nil || u.Scheme != "spiffe" || u.Host == "" || p.Tenant == "" || len(p.Namespaces) == 0 {
				return errors.New("invalid principal mapping")
			}
			ns := append([]string(nil), p.Namespaces...)
			sort.Strings(ns)
			acl := strings.Join(ns, "\x00")
			if prior, ok := tenants[p.Tenant]; ok && prior != acl {
				return errors.New("identities in one tenant must have the same namespace authorization")
			}
			tenants[p.Tenant] = acl
		}
		seen := map[string]bool{}
		for _, n := range c.Workers {
			u, e := url.Parse(n.URL)
			if e != nil || u.Scheme != "https" || u.User != nil || n.Name == "" || seen[n.Name] || n.Resources.CPU <= 0 || n.Resources.MemoryMiB <= 0 {
				return errors.New("invalid worker configuration")
			}
			seen[n.Name] = true
		}
	} else if role == "node" {
		if c.NodeName == "" || c.SSFEndpoint == "" || len(c.ControlPeers) == 0 || len(c.EvidencePeers) == 0 {
			return errors.New("node identity, SSF endpoint and role peer lists required")
		}
		u, e := url.Parse(c.AgentURL)
		if e != nil || u.Scheme != "http" || (u.Hostname() != "127.0.0.1" && u.Hostname() != "localhost") {
			return errors.New("AgentENV API must be on Pod loopback")
		}
	}
	return nil
}
