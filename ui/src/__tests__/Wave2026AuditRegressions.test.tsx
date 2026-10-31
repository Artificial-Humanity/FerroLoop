import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { HubApiClient } from "../api/client";
import { RecordsBoardPane } from "../components/RecordsBoardPane";
import { SettingsModal } from "../components/SettingsModal";
import type {
  ProjectRecordItem,
  RecordsListResponse,
  RecordState,
} from "../types/api";

// Regression tests for the 2026-10-30 Wave A/B audit (docs/audits/2026-10-30-wave-ab-cloud-audit.md).

function record(
  project: string,
  n: number,
  title: string,
  state: RecordState = "todo",
): ProjectRecordItem {
  return {
    id: `urn:fl:record:${project}-${n}`,
    handle: n,
    display_id: String(n),
    project,
    title,
    state,
    area: null,
    tier: "local",
    findings_open: 0,
    findings_total: 0,
  };
}

function board(
  project: string,
  records: ProjectRecordItem[],
): RecordsListResponse {
  return {
    project,
    available_projects: ["ferroloop", "sonora"],
    records,
    summary: {
      todo_count: records.filter((r) => r.state === "todo").length,
      doing_count: records.filter((r) => r.state === "doing").length,
      done_count: records.filter((r) => r.state === "done").length,
      total_count: records.length,
      completion_pct: 0,
      open_findings_count: 0,
      attempts_count: 0,
      total_cost_usd: 0,
    },
  };
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((r) => {
    resolve = r;
  });
  return { promise, resolve };
}

function cardTitles(): string[] {
  return screen
    .queryAllByTestId(/^record-card-/)
    .map((el) => el.textContent ?? "");
}

async function switchProject(name: string) {
  await act(async () => {
    fireEvent.change(screen.getByTestId("board-project-select"), {
      target: { value: name },
    });
  });
}

describe("RecordsBoardPane — answers that arrive after the user left the project", () => {
  it("keeps the newest project's board when an older load resolves last", async () => {
    const client = new HubApiClient({ baseUrl: "http://localhost:9000" });
    const first = deferred<RecordsListResponse>();
    const second = deferred<RecordsListResponse>();
    const third = deferred<RecordsListResponse>();
    const calls: string[] = [];
    const pending = [first, second, third];
    vi.spyOn(client, "listRecords").mockImplementation((project?: string) => {
      calls.push(project ?? "");
      return pending[calls.length - 1].promise;
    });

    render(<RecordsBoardPane selectedProject="ferroloop" apiClient={client} />);
    await waitFor(() => expect(calls).toEqual(["ferroloop"]));
    await act(async () => {
      first.resolve(board("ferroloop", [record("ferroloop", 1, "ferro-initial")]));
    });
    await waitFor(() => expect(cardTitles().join()).toContain("ferro-initial"));

    await switchProject("sonora");
    await waitFor(() => expect(calls).toEqual(["ferroloop", "sonora"]));
    await switchProject("ferroloop");
    await waitFor(() => expect(calls).toEqual(["ferroloop", "sonora", "ferroloop"]));

    // The newest request answers first, the abandoned `sonora` one last.
    await act(async () => {
      third.resolve(board("ferroloop", [record("ferroloop", 1, "ferro-fresh")]));
    });
    await act(async () => {
      second.resolve(board("sonora", [record("sonora", 7, "sonora-stale")]));
    });

    expect(screen.getByTestId("board-project-select")).toHaveValue("ferroloop");
    const titles = cardTitles().join();
    expect(titles).toContain("ferro-fresh");
    expect(titles).not.toContain("sonora-stale");
  });

  it("does not let an abandoned load's failure or completion clear the newer load", async () => {
    const client = new HubApiClient({ baseUrl: "http://localhost:9000" });
    const slowFail = deferred<RecordsListResponse>();
    const current = deferred<RecordsListResponse>();
    const answers = [
      Promise.resolve(board("ferroloop", [record("ferroloop", 1, "ferro")])),
      slowFail.promise,
      current.promise,
    ];
    let n = 0;
    vi.spyOn(client, "listRecords").mockImplementation(() => answers[n++]);

    render(<RecordsBoardPane selectedProject="ferroloop" apiClient={client} />);
    await waitFor(() => expect(cardTitles().join()).toContain("ferro"));
    await switchProject("sonora");
    await switchProject("ferroloop");
    await act(async () => {
      slowFail.resolve(Promise.reject(new Error("sonora exploded")) as never);
      await Promise.resolve();
    });
    // The abandoned load's error is not shown for the project now on screen.
    expect(screen.queryByTestId("records-board-error")).not.toBeInTheDocument();
    await act(async () => {
      current.resolve(board("ferroloop", [record("ferroloop", 1, "ferro-now")]));
    });
    expect(cardTitles().join()).toContain("ferro-now");
  });

  it("does not add a record created in one project to the board of another", async () => {
    const client = new HubApiClient({ baseUrl: "http://localhost:9000" });
    vi.spyOn(client, "listRecords").mockImplementation(async (project?: string) =>
      project === "sonora"
        ? board("sonora", [record("sonora", 1, "sonora-card")])
        : board("ferroloop", [record("ferroloop", 1, "ferro-card")]),
    );
    const created = deferred<{ record: ProjectRecordItem }>();
    vi.spyOn(client, "createRecord").mockReturnValue(created.promise);

    render(<RecordsBoardPane selectedProject="ferroloop" apiClient={client} />);
    await waitFor(() => expect(cardTitles().join()).toContain("ferro-card"));
    fireEvent.click(screen.getByTestId("board-new-record-btn"));
    fireEvent.change(screen.getByTestId("new-record-title-input"), {
      target: { value: "made in ferroloop" },
    });
    fireEvent.click(screen.getByTestId("submit-new-record-btn"));

    await switchProject("sonora");
    await waitFor(() => expect(cardTitles().join()).toContain("sonora-card"));
    await act(async () => {
      created.resolve({ record: record("ferroloop", 9, "made in ferroloop") });
    });

    expect(screen.getByTestId("board-project-select")).toHaveValue("sonora");
    expect(cardTitles().join()).not.toContain("made in ferroloop");
    expect(cardTitles().join()).toContain("sonora-card");
  });

  it("does not apply a move to a same-numbered card of the project now shown", async () => {
    const client = new HubApiClient({ baseUrl: "http://localhost:9000" });
    // Both projects number their first record "1".
    vi.spyOn(client, "listRecords").mockImplementation(async (project?: string) =>
      project === "sonora"
        ? board("sonora", [record("sonora", 1, "sonora-one", "todo")])
        : board("ferroloop", [record("ferroloop", 1, "ferro-one", "todo")]),
    );
    const moved = deferred<{ ok: boolean; record: ProjectRecordItem }>();
    vi.spyOn(client, "moveRecord").mockReturnValue(moved.promise as never);

    render(<RecordsBoardPane selectedProject="ferroloop" apiClient={client} />);
    await waitFor(() => expect(cardTitles().join()).toContain("ferro-one"));
    fireEvent.click(screen.getByTestId("move-record-1-done"));

    await switchProject("sonora");
    await waitFor(() => expect(cardTitles().join()).toContain("sonora-one"));
    await act(async () => {
      moved.resolve({
        ok: true,
        record: record("ferroloop", 1, "ferro-one", "done"),
      });
    });

    const todo = screen.getByTestId("swimlane-todo");
    expect(within(todo).getByTestId("record-card-1")).toHaveTextContent(
      "sonora-one",
    );
    expect(cardTitles().join()).not.toContain("ferro-one");
  });
});

describe("SettingsModal — TOTP enrollment never shows a secret the hub did not issue", () => {
  async function openSection(client: HubApiClient) {
    vi.spyOn(client, "listOperators").mockResolvedValue({ operators: [] });
    render(<SettingsModal isOpen={true} onClose={vi.fn()} apiClient={client} />);
    return screen.getByTestId("settings-operators-section");
  }

  it.each([
    ["an empty object", {}],
    ["null", null],
    ["a secret that is blank", { secret_base32: " ", otpauth_uri: "otpauth://x" }],
    ["a response with no otpauth uri", { secret_base32: "ABCDEF234567" }],
  ])("refuses %s as an enrollment", async (_name, answer) => {
    const client = new HubApiClient({ baseUrl: "http://localhost:9000" });
    vi.spyOn(client, "enrollTotp").mockResolvedValue(answer as never);
    const section = await openSection(client);
    fireEvent.click(within(section).getByTestId("enroll-totp-btn"));
    await waitFor(() => expect(client.enrollTotp).toHaveBeenCalled());
    await waitFor(() =>
      expect(document.body.textContent).toContain("enrollment was not started"),
    );
    expect(
      within(section).queryByTestId("totp-secret-display"),
    ).not.toBeInTheDocument();
    // The well-known demo secret must not appear anywhere.
    expect(document.body.textContent).not.toContain("JBSWY3DPEHPK3PXP");
  });

  it("does not mark a code verified unless the hub says ok: true", async () => {
    const client = new HubApiClient({ baseUrl: "http://localhost:9000" });
    vi.spyOn(client, "enrollTotp").mockResolvedValue({
      secret_base32: "ABCDEF234567",
      otpauth_uri: "otpauth://totp/FerroDeck:op?secret=ABCDEF234567",
    });
    const verify = vi.spyOn(client, "verifyTotp");
    const section = await openSection(client);
    fireEvent.click(within(section).getByTestId("enroll-totp-btn"));
    await within(section).findByTestId("totp-secret-display");

    for (const answer of [{}, null, { ok: false }]) {
      verify.mockResolvedValueOnce(answer as never);
      fireEvent.change(within(section).getByTestId("totp-verify-input"), {
        target: { value: "123456" },
      });
      fireEvent.click(within(section).getByTestId("totp-verify-btn"));
      await waitFor(() => expect(verify).toHaveBeenCalled());
      await act(async () => {
        await Promise.resolve();
      });
      expect(
        within(section).queryByTestId("totp-verified-badge"),
      ).not.toBeInTheDocument();
      verify.mockClear();
    }

    verify.mockResolvedValueOnce({ ok: true });
    fireEvent.click(within(section).getByTestId("totp-verify-btn"));
    expect(
      await within(section).findByTestId("totp-verified-badge"),
    ).toBeInTheDocument();
  });
});
