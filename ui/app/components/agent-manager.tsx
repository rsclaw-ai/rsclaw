import { IconButton } from "./button";
import { ErrorBoundary } from "./error";
import styles from "./agent-manager.module.scss";
import ReturnIcon from "../icons/return.svg";
import AddIcon from "../icons/add.svg";
import EditIcon from "../icons/edit.svg";
import DeleteIcon from "../icons/delete.svg";
import CloseIcon from "../icons/close.svg";
import ConfirmIcon from "../icons/confirm.svg";
import ReloadIcon from "../icons/reload.svg";
import { useNavigate } from "react-router-dom";
import { useEffect, useState, useCallback } from "react";
import { Path } from "../constant";
import { showConfirm, showToast } from "./ui-lib";

const GATEWAY_BASE = "http://localhost:18888/api/v1";

interface Agent {
  id: string;
  model: string;
  toolset: string[];
  channels: string[];
  status: string;
  system_prompt?: string;
}

interface AgentFormData {
  id: string;
  model: string;
  toolset: string;
  channels: string;
  system_prompt: string;
}

const EMPTY_FORM: AgentFormData = {
  id: "",
  model: "",
  toolset: "",
  channels: "",
  system_prompt: "",
};

export function AgentManagerPage() {
  const navigate = useNavigate();
  const [agents, setAgents] = useState<Agent[]>([]);
  const [loading, setLoading] = useState(true);
  const [showForm, setShowForm] = useState(false);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [form, setForm] = useState<AgentFormData>(EMPTY_FORM);
  const [saving, setSaving] = useState(false);

  const fetchAgents = useCallback(async () => {
    try {
      const res = await fetch(`${GATEWAY_BASE}/status`, {
        signal: AbortSignal.timeout(3000),
      });
      if (!res.ok) throw new Error("Failed to fetch agents");
      const data = await res.json();
      setAgents(data.agents || []);
    } catch {
      setAgents([]);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    fetchAgents();
  }, [fetchAgents]);

  const updateForm = (partial: Partial<AgentFormData>) => {
    setForm((prev) => ({ ...prev, ...partial }));
  };

  const handleAdd = () => {
    setEditingId(null);
    setForm(EMPTY_FORM);
    setShowForm(true);
  };

  const handleEdit = (agent: Agent) => {
    setEditingId(agent.id);
    setForm({
      id: agent.id,
      model: agent.model,
      toolset: agent.toolset.join(", "),
      channels: agent.channels.join(", "),
      system_prompt: agent.system_prompt || "",
    });
    setShowForm(true);
  };

  const handleDelete = async (id: string) => {
    if (await showConfirm(`Delete agent "${id}"?`)) {
      try {
        const res = await fetch(`${GATEWAY_BASE}/agents/${id}`, {
          method: "DELETE",
        });
        if (!res.ok) throw new Error("Failed to delete agent");
        showToast("Agent deleted");
        fetchAgents();
      } catch {
        showToast("Failed to delete agent");
      }
    }
  };

  const handleSave = async () => {
    if (saving) return;
    if (!form.id || !form.model) {
      showToast("ID and Model are required");
      return;
    }
    setSaving(true);

    const payload = {
      id: form.id,
      model: form.model,
      toolset: form.toolset
        .split(",")
        .map((s) => s.trim())
        .filter(Boolean),
      channels: form.channels
        .split(",")
        .map((s) => s.trim())
        .filter(Boolean),
      system_prompt: form.system_prompt,
    };

    try {
      const method = editingId ? "PUT" : "POST";
      const url = editingId
        ? `${GATEWAY_BASE}/agents/${editingId}`
        : `${GATEWAY_BASE}/agents`;
      const res = await fetch(url, {
        method,
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(payload),
      });
      if (!res.ok) throw new Error("Failed to save agent");
      showToast(editingId ? "Agent updated" : "Agent created");
      setShowForm(false);
      fetchAgents();
    } catch {
      showToast("Failed to save agent. Is the gateway running?");
    } finally {
      setSaving(false);
    }
  };

  return (
    <ErrorBoundary>
      <div className={styles["agent-manager-page"]}>
        <div className="window-header" data-tauri-drag-region>
          <div className="window-header-title">
            <div className="window-header-main-title">Agent Manager</div>
            <div className="window-header-sub-title">
              {agents.length} agent{agents.length !== 1 ? "s" : ""} configured
            </div>
          </div>
          <div className="window-actions">
            <div className="window-action-button">
              <IconButton
                icon={<ReloadIcon />}
                bordered
                onClick={fetchAgents}
              />
            </div>
            <div className="window-action-button">
              <IconButton
                icon={<ReturnIcon />}
                bordered
                onClick={() => navigate(Path.Home)}
              />
            </div>
          </div>
        </div>

        <div className={styles["agent-manager-page-body"]}>
          <div className={styles.toolbar}>
            <div className={styles["toolbar-title"]}>
              {loading ? "Loading..." : `${agents.length} agents`}
            </div>
            <IconButton
              icon={<AddIcon />}
              text="Add Agent"
              bordered
              onClick={handleAdd}
            />
          </div>

          {showForm && (
            <div className={styles["form-overlay"]}>
              <div className={styles["form-title"]}>
                {editingId ? `Edit Agent: ${editingId}` : "New Agent"}
              </div>
              <div className={styles["form-group"]}>
                <div className={styles["form-label"]}>Agent ID</div>
                <input
                  type="text"
                  placeholder="e.g., default, assistant-1"
                  value={form.id}
                  onChange={(e) => updateForm({ id: e.target.value })}
                  disabled={!!editingId}
                />
              </div>
              <div className={styles["form-group"]}>
                <div className={styles["form-label"]}>Model</div>
                <input
                  type="text"
                  placeholder="e.g., claude-sonnet-4-20250514"
                  value={form.model}
                  onChange={(e) => updateForm({ model: e.target.value })}
                />
              </div>
              <div className={styles["form-group"]}>
                <div className={styles["form-label"]}>
                  Toolset (comma-separated)
                </div>
                <input
                  type="text"
                  placeholder="e.g., read, write, exec, web_search"
                  value={form.toolset}
                  onChange={(e) => updateForm({ toolset: e.target.value })}
                />
              </div>
              <div className={styles["form-group"]}>
                <div className={styles["form-label"]}>
                  Channels (comma-separated)
                </div>
                <input
                  type="text"
                  placeholder="e.g., feishu, wechat, cli"
                  value={form.channels}
                  onChange={(e) => updateForm({ channels: e.target.value })}
                />
              </div>
              <div className={styles["form-group"]}>
                <div className={styles["form-label"]}>System Prompt</div>
                <textarea
                  placeholder="Optional system prompt for this agent..."
                  value={form.system_prompt}
                  onChange={(e) =>
                    updateForm({ system_prompt: e.target.value })
                  }
                />
              </div>
              <div className={styles["form-actions"]}>
                <IconButton
                  icon={<CloseIcon />}
                  text="Cancel"
                  bordered
                  onClick={() => setShowForm(false)}
                />
                <IconButton
                  icon={<ConfirmIcon />}
                  text={saving ? "Saving..." : "Save"}
                  bordered
                  disabled={saving}
                  onClick={handleSave}
                />
              </div>
            </div>
          )}

          {agents.length > 0 ? (
            <div className={styles["agent-list"]}>
              {agents.map((agent) => (
                <div key={agent.id} className={styles["agent-card"]}>
                  <div className={styles["agent-card-header"]}>
                    <div className={styles["agent-card-title"]}>
                      <div className={styles["agent-name"]}>
                        {agent.id}
                        <span
                          className={`${styles["agent-status-badge"]} ${
                            styles[agent.status] || styles.idle
                          }`}
                        >
                          {agent.status}
                        </span>
                      </div>
                      <div className={styles["agent-id"]}>
                        Model: {agent.model}
                      </div>
                    </div>
                    <div className={styles["agent-card-actions"]}>
                      <IconButton
                        icon={<EditIcon />}
                        bordered
                        onClick={() => handleEdit(agent)}
                      />
                      <IconButton
                        icon={<DeleteIcon />}
                        bordered
                        onClick={() => handleDelete(agent.id)}
                      />
                    </div>
                  </div>
                  <div className={styles["agent-card-details"]}>
                    {agent.toolset.length > 0 && (
                      <div className={styles["detail-item"]}>
                        <span className={styles["detail-label"]}>Tools:</span>
                        <span className={styles["detail-value"]}>
                          {agent.toolset.join(", ")}
                        </span>
                      </div>
                    )}
                    {agent.channels.length > 0 && (
                      <div className={styles["detail-item"]}>
                        <span className={styles["detail-label"]}>
                          Channels:
                        </span>
                        <span className={styles["detail-value"]}>
                          {agent.channels.join(", ")}
                        </span>
                      </div>
                    )}
                  </div>
                </div>
              ))}
            </div>
          ) : (
            !loading && (
              <div className={styles["empty-state"]}>
                No agents configured. Add one to get started.
              </div>
            )
          )}
        </div>
      </div>
    </ErrorBoundary>
  );
}
