import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import React from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { ChatPane, sessionDraftMemory, sessionSendErrors } from "../components/ChatPane";
import type { TranscriptTurn } from "../types/api";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: Error) => void;
  const promise = new Promise<T>((ok, fail) => { resolve = ok; reject = fail; });
  return { promise, resolve, reject };
}
const turn = (content: string, index = 0): TranscriptTurn => ({ index, role: "user", content, thinking: null, tool_calls: [] });
const input = () => screen.getByPlaceholderText(/Send a message/i) as HTMLTextAreaElement;
const send = () => fireEvent.click(screen.getByRole("button", { name: /^Send Message$/i }));
const props = (handler: () => Promise<unknown>) => ({ agentName: "alpha", sessionId: "a", turns: [] as TranscriptTurn[], onSendPrompt: handler });

beforeEach(() => { sessionDraftMemory.clear(); sessionSendErrors.clear(); window.localStorage.clear(); });
describe("optimistic prompt submission", () => {
  it("keeps an in-flight turn and prevents a duplicate after remount", async () => {
    const pending = deferred<unknown>();
    const handler = vi.fn(() => pending.promise);
    const first = render(<ChatPane {...props(handler)} />);
    fireEvent.change(input(), { target: { value: "first" } });
    send();
    first.unmount();
    render(<ChatPane {...props(handler)} />);
    expect(within(screen.getByTestId("optimistic-user-turn")).getByText("Sending...")).toBeInTheDocument();
    fireEvent.change(input(), { target: { value: "second" } });
    fireEvent.keyDown(input(), { key: "Enter" });
    expect(handler).toHaveBeenCalledTimes(1);
    await act(async () => { pending.resolve({ status: "sent", delivered: true }); await pending.promise; });
  });
  it("clears immediately and displays the sending turn, then reconciles a real-time transcript without duplication", async () => {
    const pending = deferred<unknown>();
    const onSendPrompt = vi.fn(() => pending.promise);
    const { rerender } = render(<ChatPane {...props(onSendPrompt)} />);
    fireEvent.change(input(), { target: { value: "hello world" } });
    send();
    expect(input().value).toBe("");
    expect(within(screen.getByTestId("optimistic-user-turn")).getByText("hello world")).toBeInTheDocument();
    expect(within(screen.getByTestId("optimistic-user-turn")).getByText("Sending...")).toBeInTheDocument();
    rerender(<ChatPane {...props(onSendPrompt)} turns={[turn("hello world")]} />);
    expect(screen.queryByTestId("turn-card-0")).toBeNull();
    await act(async () => { pending.resolve({ status: "sent", delivered: true }); await pending.promise; });
    expect(within(screen.getByTestId("turn-card-0")).getByText("hello world")).toBeInTheDocument();
    expect(screen.queryByTestId("optimistic-user-turn")).toBeNull();
  });
  it("restores text and attachments when sending rejects and composer is still empty", async () => {
    const pending = deferred<unknown>();
    sessionDraftMemory.set("a", { promptText: " original ", attachments: [{ name: "note.txt", media_type: "text/plain", data_base64: "eA==" }] });
    render(<ChatPane {...props(() => pending.promise)} />);
    expect(screen.getByText("note.txt")).toBeInTheDocument();
    send();
    expect(input().value).toBe("");
    expect(screen.queryByTestId("attachment-chip-list")).toBeNull();
    await act(async () => { pending.reject(new Error("offline")); await pending.promise.catch(() => {}); });
    expect(input().value).toBe(" original ");
    expect(screen.getByText("note.txt")).toBeInTheDocument();
    expect(screen.getByText("offline")).toBeInTheDocument();
    expect(screen.queryByTestId("optimistic-user-turn")).toBeNull();
  });
  it("does not overwrite a new draft on failed or successful sends", async () => {
    const pending = deferred<unknown>();
    render(<ChatPane {...props(() => pending.promise)} />);
    fireEvent.change(input(), { target: { value: "first" } }); send();
    fireEvent.change(input(), { target: { value: "new draft" } });
    await act(async () => { pending.reject(new Error("not sent")); await pending.promise.catch(() => {}); });
    expect(input().value).toBe("new draft");
    expect(sessionDraftMemory.get("a")?.promptText).toBe("new draft");
  });
  it("preserves newly attached files instead of restoring an old draft", async () => {
    const pending = deferred<unknown>();
    sessionDraftMemory.set("a", { promptText: "old", attachments: [{ name: "old.txt", media_type: "text/plain", data_base64: "eA==" }] });
    render(<ChatPane {...props(() => pending.promise)} />);
    send();
    fireEvent.change(screen.getByTestId("prompt-file-input"), { target: { files: [new File(["new"], "new.txt", { type: "text/plain" })] } });
    await screen.findByText("new.txt");
    await act(async () => { pending.reject(new Error("failed")); await pending.promise.catch(() => {}); });
    expect(screen.getByText("new.txt")).toBeInTheDocument();
    expect(screen.queryByText("old.txt")).toBeNull();
    expect(input().value).toBe("");
  });
  it("keeps a new draft when a send succeeds", async () => {
    const pending = deferred<unknown>();
    render(<ChatPane {...props(() => pending.promise)} />);
    fireEvent.change(input(), { target: { value: "first" } }); send();
    fireEvent.change(input(), { target: { value: "next" } });
    await act(async () => { pending.resolve({ status: "sent", delivered: true }); await pending.promise; });
    expect(input().value).toBe("next");
    expect(sessionDraftMemory.get("a")?.promptText).toBe("next");
  });
  it("keeps a newer target draft on an old failure after unmount", async () => {
    const pending = deferred<unknown>();
    const first = render(<ChatPane {...props(() => pending.promise)} />);
    fireEvent.change(input(), { target: { value: "old" } }); send();
    first.unmount();
    render(<ChatPane {...props(vi.fn(async () => {}))} />);
    fireEvent.change(input(), { target: { value: "new" } });
    await act(async () => { pending.reject(new Error("old send failed")); await pending.promise.catch(() => {}); });
    expect(input().value).toBe("new");
    expect(sessionDraftMemory.get("a")?.promptText).toBe("new");
  });
  it("restores a failed send to an already remounted empty composer", async () => {
    const pending = deferred<unknown>();
    sessionDraftMemory.set("a", { promptText: "old", attachments: [{ name: "old.txt", media_type: "text/plain", data_base64: "eA==" }] });
    const first = render(<ChatPane {...props(() => pending.promise)} />);
    send(); first.unmount();
    render(<ChatPane {...props(vi.fn(async () => {}))} />);
    await act(async () => { pending.reject(new Error("old send failed")); await pending.promise.catch(() => {}); });
    expect(input().value).toBe("old");
    expect(screen.getByText("old.txt")).toBeInTheDocument();
    expect(screen.getByText("old send failed")).toBeInTheDocument();
  });
  it("does not restore an old draft while a newer selected file is still loading", async () => {
    const pending = deferred<unknown>();
    const readers: FileReader[] = [];
    const OriginalReader = globalThis.FileReader;
    class DelayedReader {
      onload: ((this: FileReader, ev: ProgressEvent<FileReader>) => unknown) | null = null;
      onerror: ((this: FileReader, ev: ProgressEvent<FileReader>) => unknown) | null = null;
      result: string | null = null;
      readAsDataURL() { readers.push(this as unknown as FileReader); }
    }
    vi.stubGlobal("FileReader", DelayedReader);
    try {
      sessionDraftMemory.set("a", { promptText: "old", attachments: [{ name: "old.txt", media_type: "text/plain", data_base64: "eA==" }] });
      render(<ChatPane {...props(() => pending.promise)} />);
      send();
      fireEvent.change(screen.getByTestId("prompt-file-input"), { target: { files: [new File(["new"], "new.txt", { type: "text/plain" })] } });
      await act(async () => { pending.reject(new Error("failed")); await pending.promise.catch(() => {}); });
      expect(input().value).toBe("");
      expect(screen.queryByText("old.txt")).toBeNull();
      await act(async () => {
        const reader = readers[0];
        Object.defineProperty(reader, "result", { value: "data:text/plain;base64,bmV3" });
        reader.onload?.call(reader, new ProgressEvent("load") as ProgressEvent<FileReader>);
      });
      expect(screen.getByText("new.txt")).toBeInTheDocument();
      expect(screen.queryByText("old.txt")).toBeNull();
    } finally { vi.stubGlobal("FileReader", OriginalReader); }
  });
  it("keeps a delayed attachment with its original session when the target changes", async () => {
    const readers: FileReader[] = [];
    const OriginalReader = globalThis.FileReader;
    class DelayedReader {
      onload: ((this: FileReader, ev: ProgressEvent<FileReader>) => unknown) | null = null;
      onerror: ((this: FileReader, ev: ProgressEvent<FileReader>) => unknown) | null = null;
      result: string | null = null;
      readAsDataURL() { readers.push(this as unknown as FileReader); }
    }
    vi.stubGlobal("FileReader", DelayedReader);
    try {
      const handler = vi.fn(async () => {});
      const { rerender } = render(<ChatPane {...props(handler)} />);
      fireEvent.change(screen.getByTestId("prompt-file-input"), { target: { files: [new File(["x"], "alpha.txt", { type: "text/plain" })] } });
      rerender(<ChatPane agentName="beta" sessionId="b" turns={[]} onSendPrompt={handler} />);
      await act(async () => {
        const reader = readers[0];
        Object.defineProperty(reader, "result", { value: "data:text/plain;base64,eA==" });
        reader.onload?.call(reader, new ProgressEvent("load") as ProgressEvent<FileReader>);
      });
      expect(screen.queryByText("alpha.txt")).toBeNull();
      rerender(<ChatPane {...props(handler)} />);
      expect(screen.getByText("alpha.txt")).toBeInTheDocument();
    } finally { vi.stubGlobal("FileReader", OriginalReader); }
  });
  it("does not hide an identical earlier transcript turn while sending", async () => {
    const pending = deferred<unknown>();
    render(<ChatPane {...props(() => pending.promise)} turns={[turn("same")]} />);
    fireEvent.change(input(), { target: { value: "same" } }); send();
    expect(screen.getByTestId("turn-card-0")).toBeInTheDocument();
    expect(screen.getByTestId("optimistic-user-turn")).toBeInTheDocument();
    await act(async () => { pending.resolve({ queued: true }); await pending.promise; });
  });
  it("restores an inactive target draft after failure without leaking it to the current target", async () => {
    const pending = deferred<unknown>();
    const handler = vi.fn(() => pending.promise);
    const { rerender } = render(<ChatPane {...props(handler)} />);
    fireEvent.change(input(), { target: { value: "alpha draft" } }); send();
    rerender(<ChatPane agentName="beta" sessionId="b" turns={[]} onSendPrompt={handler} />);
    fireEvent.change(input(), { target: { value: "beta draft" } });
    await act(async () => { pending.reject(new Error("alpha failed")); await pending.promise.catch(() => {}); });
    expect(input().value).toBe("beta draft");
    expect(screen.queryByText("alpha failed")).toBeNull();
    rerender(<ChatPane {...props(handler)} />);
    expect(input().value).toBe("alpha draft");
    expect(screen.getByText("alpha failed")).toBeInTheDocument();
  });
  it("treats an explicit delivery failure as a failure and restores the draft", async () => {
    const pending = deferred<unknown>();
    render(<ChatPane {...props(() => pending.promise)} />);
    fireEvent.change(input(), { target: { value: "hello" } }); send();
    await act(async () => { pending.resolve({ status: "sent", delivered: false, delivery_error: "route unavailable" }); await pending.promise; });
    await waitFor(() => expect(input().value).toBe("hello"));
    expect(screen.getByText(/route unavailable/)).toBeInTheDocument();
    expect(screen.queryByTestId("optimistic-user-turn")).toBeNull();
  });
  it("removes the in-flight turn when the request is queued", async () => {
    const pending = deferred<unknown>();
    render(<ChatPane {...props(() => pending.promise)} />);
    fireEvent.change(input(), { target: { value: "queued message" } }); send();
    expect(screen.getByTestId("optimistic-user-turn")).toBeInTheDocument();
    await act(async () => { pending.resolve({ status: "queued", queued: true }); await pending.promise; });
    expect(screen.queryByTestId("optimistic-user-turn")).toBeNull();
    expect(input().value).toBe("");
  });
});
