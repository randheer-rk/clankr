package clankr

import (
	"encoding/json"
	"errors"
	"net"
	"testing"
)

// The framing and call/reply plumbing against an in-memory "node".
func TestFramesAndCalls(t *testing.T) {
	a, b := net.Pipe()
	agent, node := &conn{c: a}, &conn{c: b}
	c := &Client{k: agent, pending: map[uint64]chan reply{}}
	go func() {
		f, _ := node.recv()
		var op map[string]any
		_ = json.Unmarshal(f["op"], &op)
		if op["op"] == "contextGet" {
			_ = node.send(frame{"type": "reply", "callId": json.RawMessage(f["callId"]), "error": frame{"code": "Forbidden", "message": "not bound"}})
		}
	}()
	go func() {
		f, _ := agent.recv()
		c.resolve(f)
	}()
	_, err := c.Context("memory").Get(t.Context(), "k")
	var ce *Error
	if !errors.As(err, &ce) || ce.Code != "Forbidden" {
		t.Fatalf("want Forbidden, got %v", err)
	}
	if !errors.As(Permanent(errors.New("x")), new(permanent)) {
		t.Fatal("Permanent must be detectable")
	}
}
