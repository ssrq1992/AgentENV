//go:build !ssf

package main

import (
	scheduler "agentenv/services/scheduler/internal"
	"context"
	"errors"
	"os"
)

func initializeStorage(context.Context, *scheduler.AtomicNodeRegistry) (scheduler.ControlPlugin, func(), error) {
	if os.Getenv("AENV_SSF_CONFIG") != "" {
		return nil, nil, errors.New("SSF configured but scheduler was built without -tags ssf")
	}
	return nil, func() {}, nil
}
