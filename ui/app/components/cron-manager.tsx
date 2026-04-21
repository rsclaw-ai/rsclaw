import { IconButton } from "./button";
import { ErrorBoundary } from "./error";
import styles from "./cron-manager.module.scss";
import ReturnIcon from "../icons/return.svg";
import AddIcon from "../icons/add.svg";
import EditIcon from "../icons/edit.svg";
import DeleteIcon from "../icons/delete.svg";
import CloseIcon from "../icons/close.svg";
import ConfirmIcon from "../icons/confirm.svg";
import ReloadIcon from "../icons/reload.svg";
import PlayIcon from "../icons/play.svg";
import { useNavigate } from "react-router-dom";
import { useEffect, useState, useCallback } from "react";
import { Path } from "../constant";
import { showConfirm, showToast } from "./ui-lib";

const GATEWAY_BASE = "http://localhost:18888/api/v1";

interface CronDelivery {
  channel?: string;
  to?: string;
  mode?: string;
}

interface CronJob {
  id: string;
  name: string;
  schedule: string;
  message: string;
  timezone: string;
  enabled: boolean;
  status: string;
  last_run?: string;
  next_run?: string;
  run_count: number;
  delivery?: CronDelivery;
}

interface RunHistoryItem {
  timestamp: string;
  result: string;
  duration_ms?: number;
}

interface CronFormData {
  id: string;
  name: string;
  schedule: string;
  message: string;
  timezone: string;
  deliveryChannel: string;
  deliveryTo: string;
}

const EMPTY_FORM: CronFormData = {
  id: "",
  name: "",
  schedule: "",
  message: "",
  timezone: "Asia/Shanghai",
  deliveryChannel: "",
  deliveryTo: "",
};

export function CronManagerPage() {
  const navigate = useNavigate();
  const [jobs, setJobs] = useState<CronJob[]>([]);
  const [loading, setLoading] = useState(true);
  const [showForm, setShowForm] = useState(false);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [form, setForm] = useState<CronFormData>(EMPTY_FORM);
  const [historyJobId, setHistoryJobId] = useState<string | null>(null);
  const [history, setHistory] = useState<RunHistoryItem[]>([]);

  const fetchJobs = useCallback(async () => {
    try {
      const res = await fetch(`${GATEWAY_BASE}/cron`, {
        signal: AbortSignal.timeout(3000),
      });
      if (!res.ok) throw new Error("Failed to fetch cron jobs");
      const data = await res.json();
      setJobs(data.jobs || []);
    } catch {
      setJobs([]);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    fetchJobs();
  }, [fetchJobs]);

  const fetchHistory = async (jobId: string) => {
    try {
      const res = await fetch(`${GATEWAY_BASE}/cron/${jobId}/history`, {
        signal: AbortSignal.timeout(3000),
      });
      if (!res.ok) throw new Error("Failed to fetch history");
      const data = await res.json();
      setHistory(data.history || []);
      setHistoryJobId(jobId);
    } catch {
      setHistory([]);
      setHistoryJobId(jobId);
    }
  };

  const updateForm = (partial: Partial<CronFormData>) => {
    setForm((prev) => ({ ...prev, ...partial }));
  };

  const handleAdd = () => {
    setEditingId(null);
    setForm(EMPTY_FORM);
    setShowForm(true);
  };

  const handleEdit = (job: CronJob) => {
    setEditingId(job.id);
    setForm({
      id: job.id,
      name: job.name,
      schedule: job.schedule,
      message: job.message,
      timezone: job.timezone,
      deliveryChannel: job.delivery?.channel || "",
      deliveryTo: job.delivery?.to || "",
    });
    setShowForm(true);
  };

  const handleDelete = async (id: string) => {
    if (await showConfirm(`Delete cron job "${id}"?`)) {
      try {
        const res = await fetch(`${GATEWAY_BASE}/cron/${id}`, {
          method: "DELETE",
        });
        if (!res.ok) throw new Error("Failed to delete job");
        showToast("Cron job deleted");
        fetchJobs();
      } catch {
        showToast("Failed to delete cron job");
      }
    }
  };

  const handleToggle = async (job: CronJob) => {
    try {
      const res = await fetch(`${GATEWAY_BASE}/cron/${job.id}`, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ enabled: !job.enabled }),
      });
      if (!res.ok) throw new Error("Failed to toggle job");
      fetchJobs();
    } catch {
      showToast("Failed to toggle cron job");
    }
  };

  const handleTrigger = async (id: string) => {
    try {
      const res = await fetch(`${GATEWAY_BASE}/cron/${id}/trigger`, {
        method: "POST",
      });
      if (!res.ok) throw new Error("Failed to trigger job");
      showToast("Job triggered");
      fetchJobs();
    } catch {
      showToast("Failed to trigger cron job");
    }
  };

  const handleSave = async () => {
    if (!form.name || !form.schedule || !form.message) {
      showToast("Name, Schedule, and Message are required");
      return;
    }

    const payload: Record<string, any> = {
      id: form.id || form.name.toLowerCase().replace(/\s+/g, "-"),
      name: form.name,
      schedule: form.schedule,
      message: form.message,
      timezone: form.timezone,
    };
    if (form.deliveryChannel) {
      payload.delivery = {
        channel: form.deliveryChannel,
        to: form.deliveryTo,
        mode: "always",
      };
    }

    try {
      const method = editingId ? "PUT" : "POST";
      const url = editingId
        ? `${GATEWAY_BASE}/cron/${editingId}`
        : `${GATEWAY_BASE}/cron`;
      const res = await fetch(url, {
        method,
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(payload),
      });
      if (!res.ok) throw new Error("Failed to save job");
      showToast(editingId ? "Cron job updated" : "Cron job created");
      setShowForm(false);
      fetchJobs();
    } catch {
      showToast("Failed to save cron job. Is the gateway running?");
    }
  };

  return (
    <ErrorBoundary>
      <div className={styles["cron-manager-page"]}>
        <div className="window-header" data-tauri-drag-region>
          <div className="window-header-title">
            <div className="window-header-main-title">Cron Jobs</div>
            <div className="window-header-sub-title">
              {jobs.length} job{jobs.length !== 1 ? "s" : ""} configured
            </div>
          </div>
          <div className="window-actions">
            <div className="window-action-button">
              <IconButton
                icon={<ReloadIcon />}
                bordered
                onClick={fetchJobs}
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

        <div className={styles["cron-manager-page-body"]}>
          <div className={styles.toolbar}>
            <div className={styles["toolbar-title"]}>
              {loading ? "Loading..." : `${jobs.length} jobs`}
            </div>
            <IconButton
              icon={<AddIcon />}
              text="Add Job"
              bordered
              onClick={handleAdd}
            />
          </div>

          {showForm && (
            <div className={styles["form-overlay"]}>
              <div className={styles["form-title"]}>
                {editingId ? `Edit Job: ${editingId}` : "New Cron Job"}
              </div>
              <div className={styles["form-group"]}>
                <div className={styles["form-label"]}>Job Name</div>
                <input
                  type="text"
                  placeholder="e.g., Daily Report"
                  value={form.name}
                  onChange={(e) => updateForm({ name: e.target.value })}
                />
              </div>
              <div className={styles["form-group"]}>
                <div className={styles["form-label"]}>Cron Expression</div>
                <input
                  type="text"
                  placeholder="e.g., 0 9 * * 1-5"
                  value={form.schedule}
                  onChange={(e) => updateForm({ schedule: e.target.value })}
                />
                <div className={styles["form-hint"]}>
                  Format: minute hour day month weekday (e.g., "0 9 * * 1-5"
                  for weekdays at 9am)
                </div>
              </div>
              <div className={styles["form-group"]}>
                <div className={styles["form-label"]}>Message</div>
                <textarea
                  placeholder="Message to send when triggered..."
                  value={form.message}
                  onChange={(e) => updateForm({ message: e.target.value })}
                />
              </div>
              <div className={styles["form-group"]}>
                <div className={styles["form-label"]}>Timezone</div>
                <input
                  type="text"
                  placeholder="e.g., Asia/Shanghai"
                  value={form.timezone}
                  onChange={(e) => updateForm({ timezone: e.target.value })}
                />
              </div>
              <div className={styles["form-group"]}>
                <div className={styles["form-label"]}>Delivery Channel</div>
                <select
                  value={form.deliveryChannel}
                  onChange={(e) => updateForm({ deliveryChannel: e.target.value })}
                >
                  <option value="">None (no push)</option>
                  <option value="telegram">Telegram</option>
                  <option value="feishu">Feishu</option>
                  <option value="weixin">WeChat</option>
                  <option value="discord">Discord</option>
                  <option value="slack">Slack</option>
                  <option value="dingtalk">DingTalk</option>
                  <option value="qq">QQ</option>
                  <option value="wecom">WeCom</option>
                </select>
              </div>
              {form.deliveryChannel && (
                <div className={styles["form-group"]}>
                  <div className={styles["form-label"]}>Delivery Target (user/group ID)</div>
                  <input
                    type="text"
                    placeholder="Target user or group ID"
                    value={form.deliveryTo}
                    onChange={(e) => updateForm({ deliveryTo: e.target.value })}
                  />
                </div>
              )}
              <div className={styles["form-actions"]}>
                <IconButton
                  icon={<CloseIcon />}
                  text="Cancel"
                  bordered
                  onClick={() => setShowForm(false)}
                />
                <IconButton
                  icon={<ConfirmIcon />}
                  text="Save"
                  bordered
                  onClick={handleSave}
                />
              </div>
            </div>
          )}

          {jobs.length > 0 ? (
            <div className={styles["job-list"]}>
              {jobs.map((job) => (
                <div
                  key={job.id}
                  className={`${styles["job-card"]} ${
                    !job.enabled ? styles.disabled : ""
                  }`}
                >
                  <div className={styles["job-card-header"]}>
                    <div className={styles["job-card-title"]}>
                      <div className={styles["job-name"]}>
                        {job.name}
                        <span
                          className={`${styles["job-status-badge"]} ${
                            styles[job.status] || styles.paused
                          }`}
                        >
                          {job.status}
                        </span>
                      </div>
                      <div className={styles["job-schedule"]}>
                        {job.schedule} ({job.timezone})
                      </div>
                    </div>
                    <div className={styles["job-card-actions"]}>
                      <IconButton
                        icon={<PlayIcon />}
                        bordered
                        onClick={() => handleTrigger(job.id)}
                        title="Trigger now"
                      />
                      <div
                        className={`${styles["toggle-switch"]} ${
                          job.enabled ? styles.enabled : ""
                        }`}
                        onClick={() => handleToggle(job)}
                      >
                        <div className={styles["toggle-knob"]} />
                      </div>
                      <IconButton
                        icon={<EditIcon />}
                        bordered
                        onClick={() => handleEdit(job)}
                      />
                      <IconButton
                        icon={<DeleteIcon />}
                        bordered
                        onClick={() => handleDelete(job.id)}
                      />
                    </div>
                  </div>
                  <div className={styles["job-card-details"]}>
                    <div className={styles["detail-item"]}>
                      <span className={styles["detail-label"]}>Message:</span>
                      <span className={styles["detail-value"]}>
                        {job.message.length > 50
                          ? job.message.slice(0, 50) + "..."
                          : job.message}
                      </span>
                    </div>
                    {job.last_run && (
                      <div className={styles["detail-item"]}>
                        <span className={styles["detail-label"]}>
                          Last run:
                        </span>
                        <span className={styles["detail-value"]}>
                          {job.last_run}
                        </span>
                      </div>
                    )}
                    {job.next_run && (
                      <div className={styles["detail-item"]}>
                        <span className={styles["detail-label"]}>
                          Next run:
                        </span>
                        <span className={styles["detail-value"]}>
                          {job.next_run}
                        </span>
                      </div>
                    )}
                    <div className={styles["detail-item"]}>
                      <span className={styles["detail-label"]}>Runs:</span>
                      <span className={styles["detail-value"]}>
                        {job.run_count}
                      </span>
                    </div>
                  </div>

                  {historyJobId === job.id && (
                    <div className={styles["history-panel"]}>
                      <div className={styles["history-title"]}>
                        Run History
                      </div>
                      {history.length > 0 ? (
                        <div className={styles["history-list"]}>
                          {history.map((item, idx) => (
                            <div key={idx} className={styles["history-item"]}>
                              <span className={styles["history-time"]}>
                                {item.timestamp}
                              </span>
                              <span
                                className={`${styles["history-result"]} ${
                                  item.result === "success"
                                    ? styles.success
                                    : styles.failure
                                }`}
                              >
                                {item.result}
                                {item.duration_ms
                                  ? ` (${item.duration_ms}ms)`
                                  : ""}
                              </span>
                            </div>
                          ))}
                        </div>
                      ) : (
                        <div
                          style={{
                            fontSize: "12px",
                            opacity: 0.5,
                            textAlign: "center",
                            padding: "12px",
                          }}
                        >
                          No run history yet
                        </div>
                      )}
                    </div>
                  )}

                  <div style={{ marginTop: "8px" }}>
                    <IconButton
                      text={
                        historyJobId === job.id
                          ? "Hide History"
                          : "View History"
                      }
                      bordered
                      onClick={() => {
                        if (historyJobId === job.id) {
                          setHistoryJobId(null);
                        } else {
                          fetchHistory(job.id);
                        }
                      }}
                    />
                  </div>
                </div>
              ))}
            </div>
          ) : (
            !loading && (
              <div className={styles["empty-state"]}>
                No cron jobs configured. Add one to get started.
              </div>
            )
          )}
        </div>
      </div>
    </ErrorBoundary>
  );
}
