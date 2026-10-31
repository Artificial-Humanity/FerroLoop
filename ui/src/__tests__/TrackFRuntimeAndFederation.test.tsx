import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { HubApiClient } from "../api";
import { NodesModal } from "../components/NodesModal";
import type {
  EnrolledNode,
  FederatedTask,
  RuntimeMatrix,
} from "../types/api";

const HUB: EnrolledNode = {
  node_id: "ai-lab-0",
  display_name: "ai-lab-0",
  role: "hub",
  hostname: "ai-lab-0.local",
  os: "linux",
  arch: "x86_64",
  listen_addr: "127.0.0.1:8420",
  capabilities: ["claude"],
  labels: [],
  status: "online",
  enrolled_at_ms: 1,
  last_seen_at_ms: 2,
  environment_kind: "ai-lab",
  federated_subsystems: ["ferrowire", "ferrostate", "ferrogate", "ferrostep", "ferrodeck"],
};

const MAC: EnrolledNode = {
  node_id: "mac-studio",
  display_name: "mac-studio",
  role: "worker",
  hostname: "mac.local",
  os: "macos",
  arch: "aarch64",
  listen_addr: null,
  capabilities: ["xcode"],
  labels: [],
  status: "online",
  enrolled_at_ms: 1,
  last_seen_at_ms: 2,
  environment_kind: "macos-xcode",
  federated_subsystems: ["ferrowire"],
  active_tasks: 2,
  load_avg: 1.5,
};

const CLOUD: EnrolledNode = {
  ...MAC,
  node_id: "cloud-1",
  display_name: "cloud-1",
  os: "linux",
  environment_kind: "cloud-worker",
  federated_subsystems: [],
  active_tasks: 0,
  load_avg: null,
};

const MATRIX: RuntimeMatrix = {
  generated_at_ms: 1,
  vault_error: null,
  cli_runtimes: [
    { name: "claude", installed: true, path: "/bin/claude", version: "2.0.0", error: null, active_routes: 2, live_sessions: 1, probe_ms: 5 },
    { name: "codex", installed: false, path: null, version: null, error: "not found", active_routes: 0, live_sessions: 0, probe_ms: 1 },
  ],
  vault_providers: [
    { provider: "openai", secret_name: "OPENAI_API_KEY", provisioned: true, expired: false, status: "ready", expires_at: null },
    { provider: "xai", secret_name: "XAI_API_KEY", provisioned: false, expired: false, status: "missing", expires_at: null },
  ],
  local_engines: [
    { name: "ollama", endpoint: "127.0.0.1:11434/api/tags", reachable: true, latency_ms: 12, models: ["llama3:8b"], error: null },
    { name: "lemonade-8000", endpoint: "127.0.0.1:8000/v1/models", reachable: false, latency_ms: null, models: [], error: "connection refused" },
  ],
};

function task(over: Partial<FederatedTask> = {}): FederatedTask {
  return {
    task_id: "t-1",
    node_id: "mac-studio",
    kind: "shell",
    command: "xcodebuild test",
    project: "",
    status: "queued",
    created_at_ms: 1,
    updated_at_ms: 1,
    output: "",
    exit_code: null,
    ...over,
  };
}

function makeClient(over: Record<string, unknown> = {}) {
  const client = {
    fetchNodes: vi.fn().mockResolvedValue([HUB, MAC, CLOUD]),
    fetchRuntimeMatrix: vi.fn().mockResolvedValue(MATRIX),
    probeRuntimes: vi.fn().mockResolvedValue(MATRIX),
    fetchNodeTasks: vi.fn().mockResolvedValue([]),
    dispatchNodeTask: vi.fn().mockResolvedValue(task()),
    ...over,
  };
  return client as unknown as HubApiClient & typeof client;
}

afterEach(() => vi.restoreAllMocks());

describe("Track F: environment profiles and federation controls", () => {
  it("shows environment badges and federated subsystem pills per node", () => {
    render(<NodesModal isOpen onClose={vi.fn()} nodes={[HUB, MAC, CLOUD]} />);
    expect(screen.getByTestId("env-badge-ai-lab-0")).toHaveTextContent("ai-lab");
    expect(screen.getByTestId("env-badge-mac-studio")).toHaveTextContent("macos-xcode");
    expect(screen.getByTestId("env-badge-cloud-1")).toHaveTextContent("cloud-worker");
    for (const sub of ["ferrowire", "ferrostate", "ferrogate", "ferrostep", "ferrodeck"]) {
      expect(screen.getByTestId(`subsystem-pill-ai-lab-0-${sub}`)).toHaveTextContent(sub);
    }
    expect(screen.queryByTestId("subsystem-pill-cloud-1-ferrowire")).toBeNull();
    const mac = screen.getAllByTestId("enrolled-node-row")[1];
    expect(mac).toHaveTextContent("2 active");
    expect(mac).toHaveTextContent("load 1.50");
  });

  it("offers dispatch on workers only and keeps nodes without profile data rendering", () => {
    const legacy: EnrolledNode = { ...MAC, node_id: "legacy", environment_kind: undefined, federated_subsystems: undefined };
    render(<NodesModal isOpen onClose={vi.fn()} nodes={[HUB, MAC, legacy]} />);
    expect(screen.getAllByTestId("dispatch-node-task-btn")).toHaveLength(2);
    expect(screen.queryByTestId("env-badge-legacy")).toBeNull();
    expect(screen.queryByTestId("node-federation-ai-lab-0")).toBeNull();
  });

  it("dispatches a task through the client and lists it", async () => {
    const client = makeClient({
      dispatchNodeTask: vi.fn().mockResolvedValue(task({ task_id: "t-9", command: "cargo test" })),
    });
    render(<NodesModal isOpen onClose={vi.fn()} nodes={[HUB, MAC]} apiClient={client} />);
    const row = screen.getByTestId("node-federation-mac-studio");
    const button = within(row).getByTestId("dispatch-node-task-btn");
    expect(button).toBeDisabled();

    fireEvent.change(screen.getByTestId("dispatch-node-command-mac-studio"), {
      target: { value: "  cargo test  " },
    });
    fireEvent.change(screen.getByTestId("dispatch-node-project-mac-studio"), {
      target: { value: "ferroloop" },
    });
    expect(button).toBeEnabled();
    fireEvent.click(button);

    await waitFor(() =>
      expect(client.dispatchNodeTask).toHaveBeenCalledWith("mac-studio", {
        kind: "shell",
        command: "cargo test",
        project: "ferroloop",
      }),
    );
    const list = await screen.findByTestId("node-task-list-mac-studio");
    expect(within(list).getByTestId("node-task-row")).toHaveTextContent("cargo test");
    expect(within(list).getByTestId("node-task-row")).toHaveTextContent("queued");
    expect(screen.getByTestId("dispatch-node-command-mac-studio")).toHaveValue("");
  });

  it("surfaces a dispatch failure without dropping the draft", async () => {
    const client = makeClient({
      dispatchNodeTask: vi.fn().mockRejectedValue(new Error("node revoked")),
    });
    render(<NodesModal isOpen onClose={vi.fn()} nodes={[MAC]} apiClient={client} />);
    fireEvent.change(screen.getByTestId("dispatch-node-command-mac-studio"), {
      target: { value: "make" },
    });
    fireEvent.click(screen.getByTestId("dispatch-node-task-btn"));
    expect(await screen.findByRole("alert")).toHaveTextContent("node revoked");
    expect(screen.getByTestId("dispatch-node-command-mac-studio")).toHaveValue("make");
  });

  it("loads existing task queues for workers when a client is given", async () => {
    const client = makeClient({
      fetchNodeTasks: vi.fn().mockImplementation(async (id: string) =>
        id === "mac-studio"
          ? [task({ status: "completed", command: "ls", output: "done" })]
          : [],
      ),
    });
    render(<NodesModal isOpen onClose={vi.fn()} nodes={[HUB, MAC, CLOUD]} apiClient={client} />);
    const list = await screen.findByTestId("node-task-list-mac-studio");
    expect(list).toHaveTextContent("completed");
    expect(list).toHaveTextContent("done");
    expect(screen.queryByTestId("node-task-list-cloud-1")).toBeNull();
    expect(client.fetchNodeTasks).not.toHaveBeenCalledWith("ai-lab-0");
  });
});

describe("Track F: Federation & Runtime Matrix tab", () => {
  it("stays on the nodes tab until asked and makes no matrix request", () => {
    const client = makeClient();
    render(<NodesModal isOpen onClose={vi.fn()} nodes={[HUB]} apiClient={client} />);
    expect(screen.queryByTestId("federation-matrix-panel")).toBeNull();
    expect(client.fetchRuntimeMatrix).not.toHaveBeenCalled();
  });

  it("renders the three matrix tiers when the tab opens", async () => {
    const client = makeClient();
    render(<NodesModal isOpen onClose={vi.fn()} nodes={[HUB]} apiClient={client} />);
    fireEvent.click(screen.getByTestId("federation-tab-btn"));

    expect(await screen.findByText("CLI Runtimes")).toBeInTheDocument();
    expect(screen.getByText("Vault API Keys")).toBeInTheDocument();
    expect(screen.getByText("Local Ollama & Lemonade")).toBeInTheDocument();
    expect(client.fetchRuntimeMatrix).toHaveBeenCalledTimes(1);

    const claude = screen.getByTestId("runtime-cli-claude");
    expect(claude).toHaveTextContent("2.0.0");
    expect(claude).toHaveTextContent("2 routes");
    expect(claude).toHaveTextContent("1 live");
    expect(claude.dataset.ok).toBe("true");
    expect(screen.getByTestId("runtime-cli-codex")).toHaveTextContent("not installed");
    expect(screen.getByTestId("runtime-cli-codex").dataset.ok).toBe("false");

    expect(screen.getByTestId("runtime-vault-openai")).toHaveTextContent("ready");
    expect(screen.getByTestId("runtime-vault-openai")).toHaveTextContent("OPENAI_API_KEY");
    expect(screen.getByTestId("runtime-vault-xai")).toHaveTextContent("missing");

    expect(screen.getByTestId("runtime-local-ollama")).toHaveTextContent("12 ms");
    expect(screen.getByTestId("runtime-local-ollama")).toHaveTextContent("llama3:8b");
    expect(screen.getByTestId("runtime-local-lemonade-8000")).toHaveTextContent("connection refused");
  });

  it("re-probes on demand and disables the button while probing", async () => {
    let release: (m: RuntimeMatrix) => void = () => undefined;
    const client = makeClient({
      probeRuntimes: vi.fn().mockImplementation(
        () => new Promise<RuntimeMatrix>((resolve) => { release = resolve; }),
      ),
    });
    render(<NodesModal isOpen onClose={vi.fn()} nodes={[HUB]} apiClient={client} />);
    fireEvent.click(screen.getByTestId("federation-tab-btn"));
    await screen.findByTestId("runtime-cli-claude");

    fireEvent.click(screen.getByTestId("probe-runtimes-btn"));
    expect(screen.getByTestId("probe-runtimes-btn")).toBeDisabled();
    expect(screen.getByTestId("probe-runtimes-btn")).toHaveTextContent("Probing");
    release({ ...MATRIX, cli_runtimes: [{ ...MATRIX.cli_runtimes[0], version: "9.9.9" }] });

    await waitFor(() => expect(screen.getByTestId("runtime-cli-claude")).toHaveTextContent("9.9.9"));
    expect(screen.getByTestId("probe-runtimes-btn")).toBeEnabled();
    expect(client.probeRuntimes).toHaveBeenCalledTimes(1);
  });

  it("reports a probe failure and keeps the last results", async () => {
    const client = makeClient({
      probeRuntimes: vi.fn().mockRejectedValue(new Error("owner authority required")),
    });
    render(<NodesModal isOpen onClose={vi.fn()} nodes={[HUB]} apiClient={client} />);
    fireEvent.click(screen.getByTestId("federation-tab-btn"));
    await screen.findByTestId("runtime-cli-claude");
    fireEvent.click(screen.getByTestId("probe-runtimes-btn"));
    expect(await screen.findByTestId("matrix-error")).toHaveTextContent("owner authority required");
    expect(screen.getByTestId("runtime-cli-claude")).toBeInTheDocument();
  });

  it("shows a vault outage without hiding the other tiers", async () => {
    const client = makeClient({
      fetchRuntimeMatrix: vi.fn().mockResolvedValue({ ...MATRIX, vault_error: "vault unreadable" }),
    });
    render(<NodesModal isOpen onClose={vi.fn()} nodes={[HUB]} apiClient={client} />);
    fireEvent.click(screen.getByTestId("federation-tab-btn"));
    expect(await screen.findByText(/Vault unavailable: vault unreadable/)).toBeInTheDocument();
    expect(screen.getByTestId("runtime-local-ollama")).toBeInTheDocument();
  });
});

describe("Track F: HubApiClient federation methods", () => {
  function clientWithFetch(body: unknown) {
    const fetchMock = vi.fn().mockImplementation(
      async () =>
        new Response(JSON.stringify(body), { status: 200, headers: { "Content-Type": "application/json" } }),
    );
    vi.stubGlobal("fetch", fetchMock);
    return { client: new HubApiClient(), fetchMock };
  }

  afterEach(() => vi.unstubAllGlobals());

  it("fetchRuntimeMatrix GETs the matrix route", async () => {
    const { client, fetchMock } = clientWithFetch(MATRIX);
    expect((await client.fetchRuntimeMatrix()).cli_runtimes).toHaveLength(2);
    expect(fetchMock.mock.calls[0][0]).toBe("/api/v1/runtimes/matrix");
  });

  it("probeRuntimes POSTs an optional target", async () => {
    const { client, fetchMock } = clientWithFetch(MATRIX);
    await client.probeRuntimes();
    await client.probeRuntimes("vault");
    const [first, second] = fetchMock.mock.calls;
    expect(first[0]).toBe("/api/v1/runtimes/probe");
    expect(first[1].method).toBe("POST");
    expect(first[1].body).toBe("{}");
    expect(second[1].body).toBe(JSON.stringify({ target: "vault" }));
  });

  it("dispatchNodeTask and fetchNodeTasks encode the node id and unwrap bodies", async () => {
    const { client, fetchMock } = clientWithFetch({ task: task(), tasks: [task()] });
    expect((await client.dispatchNodeTask("a b", { command: "ls" })).task_id).toBe("t-1");
    expect(fetchMock.mock.calls[0][0]).toBe("/api/v1/nodes/a%20b/dispatch");
    expect(JSON.parse(fetchMock.mock.calls[0][1].body)).toEqual({ command: "ls" });
    expect(await client.fetchNodeTasks("a b")).toHaveLength(1);
    expect(fetchMock.mock.calls[1][0]).toBe("/api/v1/nodes/a%20b/tasks");
  });
});
