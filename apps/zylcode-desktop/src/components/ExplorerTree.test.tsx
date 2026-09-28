// Regression suite for the Explorer stale-BLOCKED defect (Live Commissioning
// §2, second surface, same class as the Terminal defect): the intelligence
// backend starting *after* the Explorer must recover into a real file tree
// without a page reload; BLOCKED must stay honest while the backend is down.

import { act, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ExplorerTree, TREE_RETRY_MS } from "./ExplorerTree";
import { fetchFileTree } from "../lib/surfaces";
import type { FileTreeResult } from "../lib/surfaces";

vi.mock("../lib/surfaces", () => ({
  fetchFileTree: vi.fn(),
}));

const fetchFileTreeMock = vi.mocked(fetchFileTree);

const REAL_TREE: FileTreeResult = {
  total_scanned: 2,
  truncated: false,
  elapsed_ms: 3,
  rows: [
    { path: "src/main.rs", language: "Rust" },
    { path: "README.md", language: "Markdown" },
  ],
};

function flush() {
  return act(async () => {
    for (let i = 0; i < 10; i++) await Promise.resolve();
  });
}

beforeEach(() => {
  vi.useFakeTimers();
  fetchFileTreeMock.mockReset();
});

afterEach(() => {
  vi.useRealTimers();
});

describe("ExplorerTree backend recovery (stale BLOCKED regression)", () => {
  it("recovers from BLOCKED to a real tree when the backend starts later — no reload", async () => {
    fetchFileTreeMock.mockResolvedValue({
      kind: "unavailable",
      reason: "start `zylcode serve-intel` to browse the repository tree",
    });

    render(<ExplorerTree />);
    await flush();

    // Honest BLOCKED while the backend is down.
    expect(screen.getByText("BLOCKED")).toBeInTheDocument();
    expect(
      screen.getByText("start `zylcode serve-intel` to browse the repository tree"),
    ).toBeInTheDocument();
    expect(screen.queryByText("main.rs")).not.toBeInTheDocument();
    const callsWhileDown = fetchFileTreeMock.mock.calls.length;

    // Still down after one retry: BLOCKED must not lie.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(TREE_RETRY_MS);
    });
    expect(screen.getByText("BLOCKED")).toBeInTheDocument();
    expect(fetchFileTreeMock.mock.calls.length).toBeGreaterThan(callsWhileDown);

    // Backend comes up → the next scheduled probe recovers the surface.
    fetchFileTreeMock.mockResolvedValue({ kind: "ready", data: REAL_TREE });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(TREE_RETRY_MS);
    });

    expect(screen.getByText("AVAILABLE")).toBeInTheDocument();
    expect(screen.getByText("2 files")).toBeInTheDocument();
    expect(screen.getByText("main.rs")).toBeInTheDocument();
    expect(screen.getByText("README.md")).toBeInTheDocument();
  });

  it("re-probes only while unavailable — a healthy tree is never re-fetched", async () => {
    fetchFileTreeMock.mockResolvedValue({ kind: "ready", data: REAL_TREE });

    render(<ExplorerTree />);
    await flush();

    expect(screen.getByText("AVAILABLE")).toBeInTheDocument();
    expect(screen.getByText("main.rs")).toBeInTheDocument();
    const callsAfterReady = fetchFileTreeMock.mock.calls.length;

    await act(async () => {
      await vi.advanceTimersByTimeAsync(TREE_RETRY_MS * 3);
    });

    expect(fetchFileTreeMock.mock.calls.length).toBe(callsAfterReady);
    expect(screen.getByText("AVAILABLE")).toBeInTheDocument();
  });

  it("keeps BLOCKED honest (no fabricated tree) while every probe fails", async () => {
    fetchFileTreeMock.mockResolvedValue({
      kind: "unavailable",
      reason: "start `zylcode serve-intel` to browse the repository tree",
    });

    render(<ExplorerTree />);
    await flush();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(TREE_RETRY_MS * 3);
    });

    expect(screen.getByText("BLOCKED")).toBeInTheDocument();
    expect(screen.queryByText("main.rs")).not.toBeInTheDocument();
    expect(fetchFileTreeMock.mock.calls.length).toBeGreaterThanOrEqual(4);
  });
});
