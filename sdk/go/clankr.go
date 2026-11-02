// Package clankr is the Clankr agent SDK for Go (agent runtime ABI v1.1). An agent talks to
// its node over $CLANKR_SOCKET: frames are a 4-byte big-endian length, then JSON. The agent
// holds no credentials; the node checks every call against the Agent spec. See sdk/python for
// the reference implementation.
package clankr

import (
	"context"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"sync"
	"time"
)

const (
	ABIMajor = 1
	ABIMinor = 1
	MaxFrame = 4 << 20
	Version  = "0.1.0"
)

// Error is a failure reported by the node or control plane. Code is stable (Forbidden,
// Conflict, QuotaExceeded, NotFound, OutcomeUnknown, ...).
type Error struct {
	Code    string `json:"code"`
	Message string `json:"message"`
}

func (e *Error) Error() string { return e.Code + ": " + e.Message }

type permanent struct{ err error }

func (p permanent) Error() string { return p.err.Error() }
func (p permanent) Unwrap() error { return p.err }

// Permanent marks a handler error as final: the task fails without retrying.
func Permanent(err error) error { return permanent{err} }

// Task is one delivery of a workflow node to this agent. Delivery is at-least-once.
type Task struct {
	RunID          string          `json:"runId"`
	Task           string          `json:"task"`
	Node           string          `json:"node"`
	Attempt        int             `json:"attempt"`
	IdempotencyKey string          `json:"idempotencyKey"`
	Input          json.RawMessage `json:"input"`
}

// Handler runs a task and returns a JSON-serializable result.
type Handler func(ctx context.Context, t *Task, c *Client) (any, error)

type frame map[string]any

type conn struct {
	c  net.Conn
	mu sync.Mutex
}

func (k *conn) send(f any) error {
	b, err := json.Marshal(f)
	if err != nil {
		return err
	}
	if len(b) > MaxFrame {
		return fmt.Errorf("frame of %d bytes exceeds %d", len(b), MaxFrame)
	}
	k.mu.Lock()
	defer k.mu.Unlock()
	var head [4]byte
	binary.BigEndian.PutUint32(head[:], uint32(len(b)))
	if _, err := k.c.Write(head[:]); err != nil {
		return err
	}
	_, err = k.c.Write(b)
	return err
}

func (k *conn) recv() (map[string]json.RawMessage, error) {
	var head [4]byte
	if _, err := io.ReadFull(k.c, head[:]); err != nil {
		return nil, err
	}
	n := binary.BigEndian.Uint32(head[:])
	if n > MaxFrame {
		return nil, fmt.Errorf("frame of %d bytes exceeds %d", n, MaxFrame)
	}
	b := make([]byte, n)
	if _, err := io.ReadFull(k.c, b); err != nil {
		return nil, err
	}
	var f map[string]json.RawMessage
	return f, json.Unmarshal(b, &f)
}

type reply struct {
	ok  json.RawMessage
	err error
}

// Client makes the calls an agent may make: models, context, tools, mailboxes.
type Client struct {
	Namespace, Agent, Instance string

	k       *conn
	mu      sync.Mutex
	next    uint64
	pending map[uint64]chan reply
}

func (c *Client) call(ctx context.Context, op frame, out any) error {
	c.mu.Lock()
	c.next++
	id := c.next
	ch := make(chan reply, 1)
	c.pending[id] = ch
	c.mu.Unlock()
	if err := c.k.send(frame{"type": "call", "callId": id, "op": op}); err != nil {
		return err
	}
	select {
	case r := <-ch:
		if r.err != nil {
			return r.err
		}
		if out == nil || len(r.ok) == 0 {
			return nil
		}
		return json.Unmarshal(r.ok, out)
	case <-ctx.Done():
		c.mu.Lock()
		delete(c.pending, id)
		c.mu.Unlock()
		return ctx.Err()
	}
}

func (c *Client) resolve(f map[string]json.RawMessage) {
	var id uint64
	_ = json.Unmarshal(f["callId"], &id)
	c.mu.Lock()
	ch, ok := c.pending[id]
	delete(c.pending, id)
	c.mu.Unlock()
	if !ok {
		return
	}
	if e := f["error"]; len(e) > 0 && string(e) != "null" {
		var ae Error
		_ = json.Unmarshal(e, &ae)
		ch <- reply{err: &ae}
		return
	}
	ch <- reply{ok: f["ok"]}
}

// Message is a chat turn for Generate.
type Message struct {
	Role    string `json:"role"`
	Content string `json:"content"`
}

type GenerateOptions struct {
	System          string
	MaxOutputTokens int
}

type Generated struct {
	Text       string `json:"text"`
	StopReason string `json:"stopReason"`
	Usage      struct {
		InputTokens  int64 `json:"inputTokens"`
		OutputTokens int64 `json:"outputTokens"`
	} `json:"usage"`
	Provider string `json:"provider"`
	Model    string `json:"model"`
}

// Generate calls a model through a ModelClaim the agent is bound to.
func (c *Client) Generate(ctx context.Context, claim string, messages []Message, o *GenerateOptions) (*Generated, error) {
	req := frame{"messages": messages}
	if o != nil && o.System != "" {
		req["system"] = o.System
	}
	if o != nil && o.MaxOutputTokens > 0 {
		req["maxOutputTokens"] = o.MaxOutputTokens
	}
	var g Generated
	return &g, c.call(ctx, frame{"op": "generate", "claim": claim, "request": req}, &g)
}

type ContextObject struct {
	Key       string          `json:"key"`
	Version   int64           `json:"version"`
	Value     json.RawMessage `json:"value"`
	Writer    string          `json:"writer"`
	UpdatedAt string          `json:"updatedAt"`
}

// ContextClaim is versioned key/value memory.
type ContextClaim struct {
	c     *Client
	Claim string
}

func (c *Client) Context(claim string) *ContextClaim { return &ContextClaim{c, claim} }

// Get returns nil, nil when the key doesn't exist.
func (cc *ContextClaim) Get(ctx context.Context, key string) (*ContextObject, error) {
	var o *ContextObject
	return o, cc.c.call(ctx, frame{"op": "contextGet", "claim": cc.Claim, "key": key}, &o)
}

// Put writes value. ifVersion: nil = overwrite, 0 = create only, N = only if currently N
// (an *Error with Code "Conflict" otherwise).
func (cc *ContextClaim) Put(ctx context.Context, key string, value any, ifVersion *int64) (*ContextObject, error) {
	op := frame{"op": "contextPut", "claim": cc.Claim, "key": key, "value": value}
	if ifVersion != nil {
		op["ifVersion"] = *ifVersion
	}
	var o ContextObject
	return &o, cc.c.call(ctx, op, &o)
}

func (cc *ContextClaim) List(ctx context.Context, prefix string) ([]ContextObject, error) {
	var l []ContextObject
	return l, cc.c.call(ctx, frame{"op": "contextList", "claim": cc.Claim, "prefix": prefix}, &l)
}

type ToolResult struct {
	Output       json.RawMessage `json:"output"`
	OutputSha256 string          `json:"outputSha256"`
	Truncated    bool            `json:"truncated"`
	DurationMs   int64           `json:"durationMs"`
	ToolVersion  string          `json:"toolVersion"`
	Replayed     bool            `json:"replayed"`
}

type ToolOptions struct {
	// IdempotencyKey overrides the key derived from the task in ctx.
	IdempotencyKey string
	Approval       string
}

type taskKey struct{}

type taskState struct {
	task *Task
	mu   sync.Mutex
	n    int
}

// CallTool calls tool.method through a ToolClaim. Inside a handler (ctx from the SDK) the
// idempotency key is derived from the task and the call's position in it, so a retried task
// gets the recorded result instead of repeating the effect.
func (c *Client) CallTool(ctx context.Context, claim, tool, method string, input any, o *ToolOptions) (*ToolResult, error) {
	if o == nil {
		o = &ToolOptions{}
	}
	key := o.IdempotencyKey
	if key == "" {
		st, ok := ctx.Value(taskKey{}).(*taskState)
		if !ok {
			return nil, &Error{"InvalidSpec", "pass ToolOptions.IdempotencyKey when calling tools outside a task"}
		}
		st.mu.Lock()
		st.n++
		key = fmt.Sprintf("%s:%s.%s:%d", st.task.IdempotencyKey, tool, method, st.n)
		st.mu.Unlock()
	}
	if input == nil {
		input = map[string]any{}
	}
	body := frame{"tool": tool, "method": method, "input": input, "idempotencyKey": key}
	if o.Approval != "" {
		body["approval"] = o.Approval
	}
	var r ToolResult
	return &r, c.call(ctx, frame{"op": "toolCall", "claim": claim, "call": body}, &r)
}

// Mailbox sends to and receives from an AgentMailbox the agent is bound to.
type Mailbox struct {
	c    *Client
	Name string
}

func (c *Client) Mailbox(name string) *Mailbox { return &Mailbox{c, name} }

type SendOptions struct {
	CorrelationID, DedupKey string
	TTLSeconds              int64
}

func (m *Mailbox) Send(ctx context.Context, payload any, o *SendOptions) (duplicate bool, err error) {
	op := frame{"op": "mailboxSend", "mailbox": m.Name, "payload": payload}
	if o != nil {
		if o.CorrelationID != "" {
			op["correlationId"] = o.CorrelationID
		}
		if o.DedupKey != "" {
			op["dedupKey"] = o.DedupKey
		}
		if o.TTLSeconds > 0 {
			op["ttlSeconds"] = o.TTLSeconds
		}
	}
	var r struct {
		Duplicate bool `json:"duplicate"`
	}
	return r.Duplicate, m.c.call(ctx, op, &r)
}

// Delivery is a leased mailbox message: Ack when done, Nack to retry.
type Delivery struct {
	m             *Mailbox
	ID            string          `json:"id"`
	Epoch         int64           `json:"leaseEpoch"`
	Sender        string          `json:"sender"`
	CorrelationID string          `json:"correlationId"`
	Payload       json.RawMessage `json:"payload"`
	Deliveries    int             `json:"deliveries"`
}

func (m *Mailbox) Receive(ctx context.Context, max int, visibility, wait time.Duration) ([]*Delivery, error) {
	var ds []*Delivery
	err := m.c.call(ctx, frame{"op": "mailboxReceive", "mailbox": m.Name, "max": max, "visibilitySeconds": int(visibility.Seconds()), "waitSeconds": int(wait.Seconds())}, &ds)
	for _, d := range ds {
		d.m = m
	}
	return ds, err
}

func (d *Delivery) Ack(ctx context.Context) error {
	return d.m.c.call(ctx, frame{"op": "mailboxAck", "mailbox": d.m.Name, "id": d.ID, "epoch": d.Epoch}, nil)
}

func (d *Delivery) Nack(ctx context.Context, delay time.Duration, reason string) error {
	return d.m.c.call(ctx, frame{"op": "mailboxNack", "mailbox": d.m.Name, "id": d.ID, "epoch": d.Epoch, "delaySeconds": delay.Seconds(), "reason": reason}, nil)
}

// Log sends a line to the node's log.
func (c *Client) Log(level, message string) error {
	return c.k.send(frame{"type": "log", "level": level, "message": message})
}

// App is an agent: register handlers, then Run.
type App struct {
	handlers  map[string]Handler
	hibernate func(context.Context, *Client) (any, error)
	// Checkpoint is the state saved before the last hibernation (nil on a fresh start);
	// CheckpointVersion is its version (0 when there is none).
	Checkpoint        json.RawMessage
	CheckpointVersion int64
}

func New() *App { return &App{handlers: map[string]Handler{}} }

// Task handles a workflow node; "*" catches every node without its own handler.
func (a *App) Task(node string, h Handler) *App {
	a.handlers[node] = h
	return a
}

// OnHibernate returns the agent's portable state when its deployment hibernates (after
// in-flight tasks finish). It comes back as App.Checkpoint when the deployment resumes.
func (a *App) OnHibernate(h func(context.Context, *Client) (any, error)) *App {
	a.hibernate = h
	return a
}

// Run connects to the node and serves tasks until told to stop. It exits the process on fatal errors.
func (a *App) Run() {
	if err := a.serve(); err != nil && !errors.Is(err, io.EOF) {
		fmt.Fprintln(os.Stderr, "clankr:", err)
		os.Exit(1)
	}
}

func (a *App) serve() error {
	path := os.Getenv("CLANKR_SOCKET")
	if path == "" {
		fmt.Fprintln(os.Stderr, "CLANKR_SOCKET is not set; agents are started by clankr-node")
		os.Exit(2)
	}
	nc, err := net.Dial("unix", path)
	if err != nil {
		return err
	}
	defer nc.Close()
	k := &conn{c: nc}
	if err := k.send(frame{"type": "hello", "abiMajor": ABIMajor, "abiMinor": ABIMinor, "sdk": "go/" + Version}); err != nil {
		return err
	}
	w, err := k.recv()
	if err != nil {
		return err
	}
	var ty string
	_ = json.Unmarshal(w["type"], &ty)
	if ty == "reject" {
		var reason string
		_ = json.Unmarshal(w["reason"], &reason)
		return fmt.Errorf("node rejected this agent: %s", reason)
	}
	c := &Client{k: k, pending: map[uint64]chan reply{}}
	_ = json.Unmarshal(w["namespace"], &c.Namespace)
	_ = json.Unmarshal(w["agent"], &c.Agent)
	_ = json.Unmarshal(w["instance"], &c.Instance)
	if cp := w["checkpoint"]; len(cp) > 0 {
		a.Checkpoint = cp
	}
	_ = json.Unmarshal(w["checkpointVersion"], &a.CheckpointVersion)
	if err := k.send(frame{"type": "ready"}); err != nil {
		return err
	}

	var running sync.WaitGroup
	stop := make(chan map[string]json.RawMessage, 1)
	// one reader for the connection's lifetime, so replies keep flowing while we wind down
	readErr := make(chan error, 1)
	go func() {
		for {
			f, err := k.recv()
			if err != nil {
				readErr <- err
				return
			}
			var t string
			_ = json.Unmarshal(f["type"], &t)
			switch t {
			case "deliver":
				running.Add(1)
				go func() { defer running.Done(); a.handle(k, c, f) }()
			case "reply":
				c.resolve(f)
			case "shutdown", "hibernate":
				select {
				case stop <- f:
				default:
				}
			}
		}
	}()
	var s map[string]json.RawMessage
	select {
	case s = <-stop:
	case err := <-readErr:
		return err
	}
	var kind string
	var graceMs int64 = 5000
	_ = json.Unmarshal(s["type"], &kind)
	_ = json.Unmarshal(s["graceMs"], &graceMs)
	if kind == "hibernate" {
		graceMs /= 2
	}
	done := make(chan struct{})
	go func() { running.Wait(); close(done) }()
	select {
	case <-done:
	case <-time.After(time.Duration(graceMs) * time.Millisecond):
	}
	if kind == "hibernate" {
		var state any
		if a.hibernate != nil {
			if state, err = a.hibernate(context.Background(), c); err != nil {
				fmt.Fprintln(os.Stderr, "clankr: hibernate hook:", err)
				state = nil
			}
		}
		return k.send(frame{"type": "checkpoint", "state": state})
	}
	return nil
}

func (a *App) handle(k *conn, c *Client, f map[string]json.RawMessage) {
	var t Task
	var delivery uint64
	_ = json.Unmarshal(f["deliveryId"], &delivery)
	for name, dst := range map[string]any{"runId": &t.RunID, "task": &t.Task, "node": &t.Node, "attempt": &t.Attempt, "idempotencyKey": &t.IdempotencyKey} {
		_ = json.Unmarshal(f[name], dst)
	}
	t.Input = f["input"]
	h := a.handlers[t.Node]
	if h == nil {
		h = a.handlers["*"]
	}
	fail := func(err error) {
		var p permanent
		retryable := !errors.As(err, &p)
		_ = k.send(frame{"type": "fail", "deliveryId": delivery, "error": err.Error(), "retryable": retryable})
	}
	if h == nil {
		fail(Permanent(fmt.Errorf("no handler for node %q", t.Node)))
		return
	}
	ctx := context.WithValue(context.Background(), taskKey{}, &taskState{task: &t})
	var result any
	var err error
	func() {
		defer func() {
			if r := recover(); r != nil {
				err = fmt.Errorf("panic: %v", r)
			}
		}()
		result, err = h(ctx, &t, c)
	}()
	if err != nil {
		fail(err)
		return
	}
	_ = k.send(frame{"type": "complete", "deliveryId": delivery, "result": result})
}
