import { IconButton } from "./button";
import { ErrorBoundary } from "./error";
import styles from "./channel-config.module.scss";
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

const CHANNEL_TYPES = [
  { id: "feishu", name: "Feishu (Lark)" },
  { id: "wechat", name: "WeChat" },
  { id: "wecom", name: "WeCom" },
  { id: "dingtalk", name: "DingTalk" },
  { id: "telegram", name: "Telegram" },
  { id: "slack", name: "Slack" },
  { id: "cli", name: "CLI" },
  { id: "http", name: "HTTP API" },
];

interface Channel {
  id: string;
  type: string;
  name: string;
  status: string;
  enabled: boolean;
  config?: Record<string, string>;
}

interface ChannelFormData {
  id: string;
  type: string;
  name: string;
  webhook_url: string;
  app_id: string;
  app_secret: string;
}

const EMPTY_FORM: ChannelFormData = {
  id: "",
  type: "feishu",
  name: "",
  webhook_url: "",
  app_id: "",
  app_secret: "",
};

export function ChannelConfigPage() {
  const navigate = useNavigate();
  const [channels, setChannels] = useState<Channel[]>([]);
  const [loading, setLoading] = useState(true);
  const [showForm, setShowForm] = useState(false);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [form, setForm] = useState<ChannelFormData>(EMPTY_FORM);

  const fetchChannels = useCallback(async () => {
    try {
      const res = await fetch(`${GATEWAY_BASE}/channels`, {
        signal: AbortSignal.timeout(3000),
      });
      if (!res.ok) throw new Error("Failed to fetch channels");
      const data = await res.json();
      setChannels(data.channels || []);
    } catch {
      setChannels([]);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    fetchChannels();
  }, [fetchChannels]);

  const updateForm = (partial: Partial<ChannelFormData>) => {
    setForm((prev) => ({ ...prev, ...partial }));
  };

  const handleAdd = () => {
    setEditingId(null);
    setForm(EMPTY_FORM);
    setShowForm(true);
  };

  const handleEdit = (channel: Channel) => {
    setEditingId(channel.id);
    setForm({
      id: channel.id,
      type: channel.type,
      name: channel.name,
      webhook_url: channel.config?.webhook_url || "",
      app_id: channel.config?.app_id || "",
      app_secret: channel.config?.app_secret || "",
    });
    setShowForm(true);
  };

  const handleDelete = async (id: string) => {
    if (await showConfirm(`Delete channel "${id}"?`)) {
      try {
        const res = await fetch(`${GATEWAY_BASE}/channels/${id}`, {
          method: "DELETE",
        });
        if (!res.ok) throw new Error("Failed to delete channel");
        showToast("Channel deleted");
        fetchChannels();
      } catch {
        showToast("Failed to delete channel");
      }
    }
  };

  const handleToggle = async (channel: Channel) => {
    try {
      const res = await fetch(`${GATEWAY_BASE}/channels/${channel.id}`, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ enabled: !channel.enabled }),
      });
      if (!res.ok) throw new Error("Failed to toggle channel");
      fetchChannels();
    } catch {
      showToast("Failed to toggle channel");
    }
  };

  const handleSave = async () => {
    if (!form.name || !form.type) {
      showToast("Name and Type are required");
      return;
    }

    const payload = {
      id: form.id || form.name.toLowerCase().replace(/\s+/g, "-"),
      type: form.type,
      name: form.name,
      config: {
        webhook_url: form.webhook_url,
        app_id: form.app_id,
        app_secret: form.app_secret,
      },
    };

    try {
      const method = editingId ? "PUT" : "POST";
      const url = editingId
        ? `${GATEWAY_BASE}/channels/${editingId}`
        : `${GATEWAY_BASE}/channels`;
      const res = await fetch(url, {
        method,
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(payload),
      });
      if (!res.ok) throw new Error("Failed to save channel");
      showToast(editingId ? "Channel updated" : "Channel created");
      setShowForm(false);
      fetchChannels();
    } catch {
      showToast("Failed to save channel. Is the gateway running?");
    }
  };

  const handleQrScan = (channelType: string) => {
    showToast(`QR scan for ${channelType} - requires Tauri desktop app`);
  };

  const needsQrScan = form.type === "feishu" || form.type === "wechat";

  return (
    <ErrorBoundary>
      <div className={styles["channel-config-page"]}>
        <div className="window-header" data-tauri-drag-region>
          <div className="window-header-title">
            <div className="window-header-main-title">Channels</div>
            <div className="window-header-sub-title">
              {channels.length} channel{channels.length !== 1 ? "s" : ""}{" "}
              configured
            </div>
          </div>
          <div className="window-actions">
            <div className="window-action-button">
              <IconButton
                icon={<ReloadIcon />}
                bordered
                onClick={fetchChannels}
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

        <div className={styles["channel-config-page-body"]}>
          <div className={styles.toolbar}>
            <div className={styles["toolbar-title"]}>
              {loading ? "Loading..." : `${channels.length} channels`}
            </div>
            <IconButton
              icon={<AddIcon />}
              text="Add Channel"
              bordered
              onClick={handleAdd}
            />
          </div>

          {showForm && (
            <div className={styles["form-overlay"]}>
              <div className={styles["form-title"]}>
                {editingId ? `Edit Channel: ${editingId}` : "New Channel"}
              </div>
              <div className={styles["form-group"]}>
                <div className={styles["form-label"]}>Channel Name</div>
                <input
                  type="text"
                  placeholder="e.g., My Feishu Bot"
                  value={form.name}
                  onChange={(e) => updateForm({ name: e.target.value })}
                />
              </div>
              <div className={styles["form-group"]}>
                <div className={styles["form-label"]}>Channel Type</div>
                <select
                  value={form.type}
                  onChange={(e) => updateForm({ type: e.target.value })}
                >
                  {CHANNEL_TYPES.map((ct) => (
                    <option key={ct.id} value={ct.id}>
                      {ct.name}
                    </option>
                  ))}
                </select>
              </div>
              <div className={styles["form-group"]}>
                <div className={styles["form-label"]}>App ID</div>
                <input
                  type="text"
                  placeholder="Application ID"
                  value={form.app_id}
                  onChange={(e) => updateForm({ app_id: e.target.value })}
                />
              </div>
              <div className={styles["form-group"]}>
                <div className={styles["form-label"]}>App Secret</div>
                <input
                  type="password"
                  placeholder="Application secret"
                  value={form.app_secret}
                  onChange={(e) => updateForm({ app_secret: e.target.value })}
                />
              </div>
              <div className={styles["form-group"]}>
                <div className={styles["form-label"]}>Webhook URL</div>
                <input
                  type="text"
                  placeholder="https://..."
                  value={form.webhook_url}
                  onChange={(e) => updateForm({ webhook_url: e.target.value })}
                />
              </div>
              {needsQrScan && (
                <div className={styles["qr-section"]}>
                  <div className={styles["qr-placeholder"]}>
                    QR Code will appear here
                  </div>
                  <IconButton
                    text="Scan QR Code"
                    bordered
                    onClick={() => handleQrScan(form.type)}
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

          {channels.length > 0 ? (
            <div className={styles["channel-list"]}>
              {channels.map((channel) => (
                <div
                  key={channel.id}
                  className={`${styles["channel-card"]} ${
                    !channel.enabled ? styles.disabled : ""
                  }`}
                >
                  <div className={styles["channel-card-header"]}>
                    <div className={styles["channel-card-title"]}>
                      <span className={styles["channel-name"]}>
                        {channel.name}
                      </span>
                      <span className={styles["channel-type"]}>
                        {channel.type}
                      </span>
                      <span
                        className={`${styles["channel-status"]} ${
                          styles[channel.status] || styles.disconnected
                        }`}
                      >
                        {channel.status}
                      </span>
                    </div>
                    <div className={styles["channel-card-actions"]}>
                      <div
                        className={`${styles["toggle-switch"]} ${
                          channel.enabled ? styles.enabled : ""
                        }`}
                        onClick={() => handleToggle(channel)}
                      >
                        <div className={styles["toggle-knob"]} />
                      </div>
                      <IconButton
                        icon={<EditIcon />}
                        bordered
                        onClick={() => handleEdit(channel)}
                      />
                      <IconButton
                        icon={<DeleteIcon />}
                        bordered
                        onClick={() => handleDelete(channel.id)}
                      />
                    </div>
                  </div>
                  {channel.config && (
                    <div className={styles["channel-card-details"]}>
                      {channel.config.app_id && (
                        <div className={styles["detail-item"]}>
                          <span className={styles["detail-label"]}>
                            App ID:
                          </span>
                          <span className={styles["detail-value"]}>
                            {channel.config.app_id}
                          </span>
                        </div>
                      )}
                    </div>
                  )}
                </div>
              ))}
            </div>
          ) : (
            !loading && (
              <div className={styles["empty-state"]}>
                No channels configured. Add one to get started.
              </div>
            )
          )}
        </div>
      </div>
    </ErrorBoundary>
  );
}
