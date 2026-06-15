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
import { useEffect, useState, useCallback, useRef, useMemo } from "react";
import { Path } from "../constant";
import { showConfirm, showToast } from "./ui-lib";
import { gatewayFetch, wechatQrStart, wechatQrStatus } from "../lib/rsclaw-api";
import { isTauri, invoke as tauriInvokeV2 } from "../utils/tauri";
import { getLang } from "../locales";
import {
  useCatalog,
  getChannelMap,
  getChannelOrder,
  type CatalogChannelDef,
} from "../lib/catalog";

// ── Channel type definitions (sourced from the defaults.toml catalog) ──

interface CredField {
  key: string;
  label: string;
  type: "text" | "password";
  placeholder: string;
}

interface ChannelTypeDef {
  id: string;
  icon: string;
  name: string;
  hasQr: boolean;
  credFields: CredField[];
}

const CUSTOM_CHANNEL_TYPE: ChannelTypeDef = {
  id: "custom", icon: "⚙", name: "Custom", hasQr: false, credFields: [
    { key: "webhookUrl", label: "Webhook URL", type: "text", placeholder: "https://..." },
    { key: "wsUrl", label: "WebSocket URL", type: "text", placeholder: "wss://..." },
  ],
};

function mapCatalogChannel(c: CatalogChannelDef, zh: boolean): ChannelTypeDef {
  return {
    id: c.id,
    icon: c.icon,
    name: zh ? c.name : c.nameEn,
    hasQr: c.hasQr,
    credFields: c.credFields.map((f) => ({
      key: f.key,
      label: f.label,
      type: (f.type === "password" ? "password" : "text") as "text" | "password",
      placeholder: f.ph,
    })),
  };
}

// Ordered channel-type list for the current language (+ the custom extra).
function getChannelTypes(): ChannelTypeDef[] {
  const zh = getLang() === "cn";
  const map = getChannelMap();
  const list = getChannelOrder(zh ? "cn" : "en")
    .map((id) => map[id])
    .filter(Boolean)
    .map((c) => mapCatalogChannel(c, zh));
  return [...list, CUSTOM_CHANNEL_TYPE];
}

function getChannelTypeMap(): Record<string, ChannelTypeDef> {
  return Object.fromEntries(getChannelTypes().map((ct) => [ct.id, ct]));
}

// ── Types ──

interface Channel {
  id: string;
  type: string;
  name: string;
  status: string;
  enabled: boolean;
  config?: Record<string, string>;
}

// ── Component ──

export function ChannelConfigPage() {
  const navigate = useNavigate();
  // Channel types from the defaults.toml catalog (seeded synchronously from
  // FALLBACK, reconciled on load).
  const { loaded: catalogLoaded } = useCatalog();
  const channelTypes = useMemo(() => getChannelTypes(), [catalogLoaded]);
  const channelTypeMap = useMemo(() => getChannelTypeMap(), [catalogLoaded]);
  const [channels, setChannels] = useState<Channel[]>([]);
  const [loading, setLoading] = useState(true);
  const [showForm, setShowForm] = useState(false);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [formType, setFormType] = useState("feishu");
  const [formName, setFormName] = useState("");
  const [formCreds, setFormCreds] = useState<Record<string, string>>({});
  const [qrUrl, setQrUrl] = useState<string | null>(null);
  const [qrPolling, setQrPolling] = useState(false);
  const qrPollRef = useRef<ReturnType<typeof setInterval> | null>(null);

  const fetchChannels = useCallback(async () => {
    try {
      const res = await gatewayFetch("/api/v1/channels");
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

  useEffect(() => {
    return () => {
      if (qrPollRef.current) clearInterval(qrPollRef.current);
    };
  }, []);

  const typeDef = channelTypeMap[formType] || channelTypes[0];

  const resetForm = () => {
    setFormName("");
    setFormType("feishu");
    setFormCreds({});
    setQrUrl(null);
    setQrPolling(false);
  };

  const handleAdd = () => {
    setEditingId(null);
    resetForm();
    setShowForm(true);
  };

  const handleEdit = (channel: Channel) => {
    setEditingId(channel.id);
    setFormName(channel.name);
    setFormType(channel.type);
    setFormCreds(channel.config || {});
    setQrUrl(null);
    setQrPolling(false);
    setShowForm(true);
  };

  const handleDelete = async (id: string) => {
    if (await showConfirm(`Delete channel "${id}"?`)) {
      try {
        const res = await gatewayFetch(`/api/v1/channels/${id}`, {
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
      const res = await gatewayFetch(`/api/v1/channels/${channel.id}`, {
        method: "PUT",
        body: JSON.stringify({ enabled: !channel.enabled }),
      });
      if (!res.ok) throw new Error("Failed to toggle channel");
      fetchChannels();
    } catch {
      showToast("Failed to toggle channel");
    }
  };

  const handleSave = async () => {
    if (!formName || !formType) {
      showToast("Name and Type are required");
      return;
    }

    const payload = {
      id: editingId || formName.toLowerCase().replace(/\s+/g, "-"),
      type: formType,
      name: formName,
      config: { ...formCreds },
    };

    try {
      const method = editingId ? "PUT" : "POST";
      const url = editingId
        ? `/api/v1/channels/${editingId}`
        : "/api/v1/channels";
      const res = await gatewayFetch(url, {
        method,
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

  // ── QR Login (WeChat / Feishu) ──
  //
  // In Tauri desktop, both channels use the rsclaw sidecar with --quiet so the
  // QR PNG and credentials are written to disk silently. In the browser we
  // fall back to the HTTP wechat endpoint; Feishu has no HTTP endpoint yet.

  const startTauriQrLogin = async (channel: string) => {
    setQrPolling(true);
    setQrUrl(null);

    if (qrPollRef.current) clearInterval(qrPollRef.current);

    try {
      await tauriInvokeV2("channel_login_start", { channel });
    } catch {
      setQrPolling(false);
      showToast("Failed to start QR login");
      return;
    }

    let attempts = 0;
    let qrFound = false;
    qrPollRef.current = setInterval(async () => {
      attempts++;
      try {
        const status: string = await tauriInvokeV2("channel_login_status");
        if (status === "done") {
          if (qrPollRef.current) clearInterval(qrPollRef.current);
          setQrUrl(null);
          setQrPolling(false);
          showToast(channel === "wechat" ? "WeChat login successful!" : "Feishu login successful!");
          fetchChannels();
          setShowForm(false);
          return;
        }
        if (!qrFound) {
          const dataUri: string | null = await tauriInvokeV2("channel_login_qr");
          if (dataUri) {
            qrFound = true;
            setQrUrl(dataUri);
          }
        }
      } catch {
        // keep polling
      }
      if (attempts > 60) {
        if (qrPollRef.current) clearInterval(qrPollRef.current);
        setQrPolling(false);
        if (!qrFound) showToast("QR login timed out");
      }
    }, 2000);
  };

  const handleQrLogin = async () => {
    if (isTauri) {
      await startTauriQrLogin(formType);
      return;
    }
    if (formType === "wechat") {
      try {
        const data = await wechatQrStart();
        if (data.qrcode_url) {
          setQrUrl(data.qrcode_url);
          pollWechatQr(data.qrcode_token);
        } else {
          showToast("Failed to get QR code");
        }
      } catch {
        showToast("QR login requires the gateway to be running");
      }
    } else if (formType === "feishu") {
      showToast("Feishu QR login requires the desktop app");
    }
  };

  const pollWechatQr = async (token: string) => {
    setQrPolling(true);
    for (let i = 0; i < 60; i++) {
      await new Promise((r) => setTimeout(r, 2000));
      try {
        const data = await wechatQrStatus(token);
        if (data.status === "ok") {
          showToast("WeChat login successful!");
          setQrUrl(null);
          setQrPolling(false);
          fetchChannels();
          setShowForm(false);
          return;
        }
      } catch {
        break;
      }
    }
    setQrPolling(false);
  };

  // ── Render ──

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
                <div className={styles["form-label"]}>Channel Type</div>
                <select
                  value={formType}
                  onChange={(e) => {
                    setFormType(e.target.value);
                    setFormCreds({});
                    setQrUrl(null);
                  }}
                >
                  {channelTypes.map((ct) => (
                    <option key={ct.id} value={ct.id}>
                      {ct.icon} {ct.name}
                    </option>
                  ))}
                </select>
              </div>
              <div className={styles["form-group"]}>
                <div className={styles["form-label"]}>Channel Name</div>
                <input
                  type="text"
                  placeholder={`e.g., My ${typeDef.name} Bot`}
                  value={formName}
                  onChange={(e) => setFormName(e.target.value)}
                />
              </div>

              {/* Dynamic credential fields based on channel type */}
              {typeDef.credFields.map((field) => (
                <div className={styles["form-group"]} key={field.key}>
                  <div className={styles["form-label"]}>{field.label}</div>
                  <input
                    type={field.type}
                    placeholder={field.placeholder}
                    value={formCreds[field.key] || ""}
                    onChange={(e) =>
                      setFormCreds((prev) => ({
                        ...prev,
                        [field.key]: e.target.value,
                      }))
                    }
                  />
                </div>
              ))}

              {/* QR login section for supported channels */}
              {typeDef.hasQr && (
                <div className={styles["qr-section"]}>
                  {qrUrl ? (
                    <img
                      src={qrUrl}
                      alt="QR Code"
                      style={{ width: 200, height: 200, borderRadius: 8 }}
                    />
                  ) : (
                    <div className={styles["qr-placeholder"]}>
                      QR Code will appear here
                    </div>
                  )}
                  <IconButton
                    text={qrPolling ? "Waiting for scan..." : "Scan QR Code"}
                    bordered
                    onClick={handleQrLogin}
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
              {channels.map((channel) => {
                const ct = channelTypeMap[channel.type];
                return (
                  <div
                    key={channel.id}
                    className={`${styles["channel-card"]} ${
                      !channel.enabled ? styles.disabled : ""
                    }`}
                  >
                    <div className={styles["channel-card-header"]}>
                      <div className={styles["channel-card-title"]}>
                        <span className={styles["channel-icon"]}>
                          {ct?.icon || "\u2699"}
                        </span>
                        <span className={styles["channel-name"]}>
                          {channel.name}
                        </span>
                        <span className={styles["channel-type"]}>
                          {ct?.name || channel.type}
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
                        {ct?.credFields
                          .filter((f) => channel.config?.[f.key])
                          .map((f) => (
                            <div className={styles["detail-item"]} key={f.key}>
                              <span className={styles["detail-label"]}>
                                {f.label}:
                              </span>
                              <span className={styles["detail-value"]}>
                                {f.type === "password"
                                  ? "\u2022\u2022\u2022\u2022\u2022\u2022\u2022\u2022"
                                  : channel.config?.[f.key]}
                              </span>
                            </div>
                          ))}
                      </div>
                    )}
                  </div>
                );
              })}
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
