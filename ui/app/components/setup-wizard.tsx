import JSON5 from "json5";
import { IconButton } from "./button";
import { ErrorBoundary } from "./error";
import styles from "./setup-wizard.module.scss";
import ReturnIcon from "../icons/return.svg";
import ConfirmIcon from "../icons/confirm.svg";
import { useNavigate } from "react-router-dom";
import { useState, useEffect, useRef } from "react";
import { Path } from "../constant";
import { isTauri, invoke as tauriInvokeV2 } from "../utils/tauri";
import { showToast } from "./ui-lib";
import {
  type ApiType,
  API_TYPE_LABELS,
  API_TYPE_DEFAULT_URLS,
  API_TYPE_NEEDS_KEY,
} from "../lib/provider-defaults";

const GATEWAY_BASE = "http://localhost:18888/api/v1";

const LANGUAGES = [
  { code: "zh-CN", name: "简体中文" },
  { code: "zh-TW", name: "繁體中文" },
  { code: "en", name: "English" },
  { code: "ja", name: "日本語" },
  { code: "ko", name: "한국어" },
  { code: "fr", name: "Français" },
  { code: "de", name: "Deutsch" },
  { code: "es", name: "Español" },
  { code: "pt", name: "Português" },
  { code: "ru", name: "Русский" },
];

// Map gateway.language config values to LANGUAGES codes
const CONFIG_LANG_TO_CODE: Record<string, string> = {
  Chinese: "zh-CN",
  English: "en",
  Japanese: "ja",
  Korean: "ko",
  Thai: "th",
  Vietnamese: "vi",
  French: "fr",
  German: "de",
  Spanish: "es",
  Russian: "ru",
};

const PROVIDERS = [
  {
    id: "anthropic",
    name: "Anthropic",
    desc: "Claude models (recommended)",
    placeholder: "sk-ant-...",
  },
  {
    id: "openai",
    name: "OpenAI",
    desc: "GPT-4, GPT-3.5 models",
    placeholder: "sk-...",
  },
  {
    id: "doubao",
    name: "Doubao (\u8C46\u5305)",
    desc: "ByteDance, API URL editable",
    placeholder: "xxxxxxxx-xxxx-...",
  },
  {
    id: "qwen",
    name: "Qwen (\u5343\u95EE)",
    desc: "Alibaba Cloud DashScope",
    placeholder: "sk-...",
  },
  {
    id: "ollama",
    name: "Ollama",
    desc: "Local models, no API key needed",
    placeholder: "",
  },
  {
    id: "custom",
    name: "Custom",
    desc: "OpenAI-compatible endpoint",
    placeholder: "your-api-key",
  },
];

interface WizardConfig {
  language: string;
  provider: string;
  apiKey: string;
  baseUrl: string;
  apiType: ApiType;
  port: number;
  bindMode: string;
}

export function SetupWizardPage() {
  const navigate = useNavigate();
  const [step, setStep] = useState(0);
  const [config, setConfig] = useState<WizardConfig>({
    language: "zh-CN",
    provider: "anthropic",
    apiKey: "",
    baseUrl: "",
    apiType: "openai",
    port: 18888,
    bindMode: "localhost",
  });

  const totalSteps = 4;

  const updateConfig = (partial: Partial<WizardConfig>) => {
    setConfig((prev) => ({ ...prev, ...partial }));
  };

  // On mount: if config already has gateway.language set, skip language selection
  const langCheckRef = useRef(false);
  useEffect(() => {
    if (langCheckRef.current) return;
    langCheckRef.current = true;
    (async () => {
      try {
        const tauriInvoke = isTauri ? tauriInvokeV2 : null;
        let cfgLang: string | undefined;
        if (tauriInvoke) {
          const raw: string = await tauriInvoke("read_config_file");
          const cfg = JSON5.parse(raw || "{}");
          cfgLang = cfg?.gateway?.language;
        } else {
          const res = await fetch("http://localhost:18888/api/v1/config");
          if (res.ok) {
            const cfg = await res.json();
            cfgLang = cfg?.gateway?.language;
          }
        }
        if (cfgLang && typeof cfgLang === "string" && cfgLang.trim()) {
          const langCode = CONFIG_LANG_TO_CODE[cfgLang.trim()];
          if (langCode) {
            updateConfig({ language: langCode });
            setStep(1);
          }
        }
      } catch (e) {
        console.warn("[setup-wizard] language detection failed:", e);
      }
    })();
  }, []);

  const handleFinish = async () => {
    try {
      const providerConfig: Record<string, any> = {};
      if (config.provider === "custom") {
        providerConfig.custom = {
          api: config.apiType,
          baseUrl: config.baseUrl,
          ...(config.apiKey ? { apiKey: config.apiKey } : {}),
          enabled: true,
        };
      } else if (config.provider === "ollama") {
        providerConfig.ollama = {
          api: "ollama",
          baseUrl: config.baseUrl || "http://localhost:11434",
          enabled: true,
        };
      } else if (config.apiKey) {
        providerConfig[config.provider] = { apiKey: config.apiKey, enabled: true };
      }

      const body = {
        gateway: {
          port: config.port,
          bind: config.bindMode === "localhost" ? "loopback" : "0.0.0.0",
          language: LANGUAGES.find((l) => l.code === config.language)?.name || config.language,
        },
        models: { providers: providerConfig },
      };

      const res = await fetch(`${GATEWAY_BASE}/config`, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      });
      if (!res.ok) throw new Error("Failed to save config");
      showToast("Configuration saved successfully");
      navigate(Path.Home);
    } catch (e) {
      showToast("Failed to save configuration. Is the gateway running?");
    }
  };

  const selectedProvider = PROVIDERS.find((p) => p.id === config.provider);

  const renderStep = () => {
    switch (step) {
      case 0:
        return (
          <div className={styles["step-content"]}>
            <div className={styles["step-title"]}>Select Language</div>
            <div className={styles["step-description"]}>
              Choose your preferred language for the interface
            </div>
            <div className={styles["language-grid"]}>
              {LANGUAGES.map((lang) => (
                <div
                  key={lang.code}
                  className={`${styles["language-option"]} ${
                    config.language === lang.code ? styles.selected : ""
                  }`}
                  onClick={() => updateConfig({ language: lang.code })}
                >
                  {lang.name}
                </div>
              ))}
            </div>
          </div>
        );

      case 1:
        return (
          <div className={styles["step-content"]}>
            <div className={styles["step-title"]}>Model Provider</div>
            <div className={styles["step-description"]}>
              Select your AI model provider and enter credentials
            </div>
            <div className={styles["provider-list"]}>
              {PROVIDERS.map((provider) => (
                <div
                  key={provider.id}
                  className={`${styles["provider-option"]} ${
                    config.provider === provider.id ? styles.selected : ""
                  }`}
                  onClick={() => {
                    const updates: Partial<WizardConfig> = { provider: provider.id };
                    if (provider.id === "ollama") {
                      updates.baseUrl = "http://localhost:11434";
                    } else if (provider.id === "custom") {
                      updates.apiType = "openai";
                      updates.baseUrl = "";
                    } else {
                      updates.baseUrl = "";
                    }
                    updateConfig(updates);
                  }}
                >
                  <div>
                    <div className={styles["provider-name"]}>
                      {provider.name}
                    </div>
                    <div className={styles["provider-desc"]}>
                      {provider.desc}
                    </div>
                  </div>
                </div>
              ))}
            </div>
            {config.provider === "custom" && (
              <div className={styles["form-group"]}>
                <div className={styles["form-label"]}>API Type</div>
                <select
                  value={config.apiType}
                  onChange={(e) => {
                    const at = e.target.value as ApiType;
                    updateConfig({ apiType: at, baseUrl: "" });
                  }}
                >
                  {(Object.keys(API_TYPE_LABELS) as ApiType[]).map((at) => (
                    <option key={at} value={at}>{API_TYPE_LABELS[at]}</option>
                  ))}
                </select>
              </div>
            )}
            {config.provider === "custom" && (
              <div className={styles["form-group"]}>
                <div className={styles["form-label"]}>Base URL</div>
                <input
                  type="text"
                  placeholder="https://your-api-server.com/v1"
                  value={config.baseUrl}
                  onChange={(e) => updateConfig({ baseUrl: e.target.value })}
                />
              </div>
            )}
            {config.provider === "custom" && API_TYPE_NEEDS_KEY[config.apiType] && (
              <div className={styles["form-group"]}>
                <div className={styles["form-label"]}>API Key</div>
                <input
                  type="password"
                  placeholder="sk-..."
                  value={config.apiKey}
                  onChange={(e) => updateConfig({ apiKey: e.target.value })}
                />
              </div>
            )}
            {selectedProvider && selectedProvider.placeholder && config.provider !== "custom" && (
              <div className={styles["form-group"]}>
                <div className={styles["form-label"]}>API Key</div>
                <input
                  type="password"
                  placeholder={selectedProvider.placeholder}
                  value={config.apiKey}
                  onChange={(e) => updateConfig({ apiKey: e.target.value })}
                />
              </div>
            )}
            {config.provider === "doubao" && (
              <div className={styles["form-group"]}>
                <div className={styles["form-label"]}>API URL</div>
                <input
                  type="text"
                  placeholder="https://ark.cn-beijing.volces.com/api/v3"
                  value={config.baseUrl}
                  onChange={(e) => updateConfig({ baseUrl: e.target.value })}
                />
              </div>
            )}
          </div>
        );

      case 2:
        return (
          <div className={styles["step-content"]}>
            <div className={styles["step-title"]}>Gateway Settings</div>
            <div className={styles["step-description"]}>
              Configure port and network binding
            </div>
            <div className={styles["form-group"]}>
              <div className={styles["form-label"]}>Port</div>
              <input
                type="number"
                value={config.port}
                onChange={(e) =>
                  updateConfig({ port: parseInt(e.target.value) || 18888 })
                }
                min={1024}
                max={65535}
              />
            </div>
            <div className={styles["form-group"]}>
              <div className={styles["form-label"]}>Bind Mode</div>
              <select
                value={config.bindMode}
                onChange={(e) => updateConfig({ bindMode: e.target.value })}
              >
                <option value="localhost">
                  Localhost only (127.0.0.1) - Recommended
                </option>
                <option value="lan">LAN access (0.0.0.0)</option>
              </select>
            </div>
          </div>
        );

      case 3:
        return (
          <div className={styles["step-content"]}>
            <div className={styles["step-title"]}>Setup Complete</div>
            <div className={styles["step-description"]}>
              Review your configuration
            </div>
            <div className={styles["summary-list"]}>
              <div className={styles["summary-item"]}>
                <span className={styles["summary-label"]}>Language</span>
                <span className={styles["summary-value"]}>
                  {LANGUAGES.find((l) => l.code === config.language)?.name ||
                    config.language}
                </span>
              </div>
              <div className={styles["summary-item"]}>
                <span className={styles["summary-label"]}>Provider</span>
                <span className={styles["summary-value"]}>
                  {selectedProvider?.name || config.provider}
                </span>
              </div>
              {config.provider === "custom" && (
                <div className={styles["summary-item"]}>
                  <span className={styles["summary-label"]}>API Type</span>
                  <span className={styles["summary-value"]}>
                    {API_TYPE_LABELS[config.apiType]}
                  </span>
                </div>
              )}
              {config.provider === "custom" && (
                <div className={styles["summary-item"]}>
                  <span className={styles["summary-label"]}>Base URL</span>
                  <span className={styles["summary-value"]}>
                    {config.baseUrl || "(not set)"}
                  </span>
                </div>
              )}
              <div className={styles["summary-item"]}>
                <span className={styles["summary-label"]}>API Key</span>
                <span className={styles["summary-value"]}>
                  {config.apiKey
                    ? `${config.apiKey.slice(0, 8)}...`
                    : "(not set)"}
                </span>
              </div>
              <div className={styles["summary-item"]}>
                <span className={styles["summary-label"]}>Port</span>
                <span className={styles["summary-value"]}>{config.port}</span>
              </div>
              <div className={styles["summary-item"]}>
                <span className={styles["summary-label"]}>Bind Mode</span>
                <span className={styles["summary-value"]}>
                  {config.bindMode}
                </span>
              </div>
            </div>
          </div>
        );

      default:
        return null;
    }
  };

  return (
    <ErrorBoundary>
      <div className={styles["setup-wizard-page"]}>
        <div className="window-header" data-tauri-drag-region>
          <div className="window-header-title">
            <div className="window-header-main-title">Setup Wizard</div>
            <div className="window-header-sub-title">
              Step {step + 1} of {totalSteps}
            </div>
          </div>
          <div className="window-actions">
            <div className="window-action-button">
              <IconButton
                icon={<ReturnIcon />}
                bordered
                onClick={() => navigate(Path.Home)}
              />
            </div>
          </div>
        </div>

        <div className={styles["setup-wizard-page-body"]}>
          <div className={styles["wizard-container"]}>
            <div className={styles["step-indicator"]}>
              {Array.from({ length: totalSteps }).map((_, i) => (
                <div
                  key={i}
                  className={`${styles["step-dot"]} ${
                    i === step
                      ? styles.active
                      : i < step
                        ? styles.completed
                        : ""
                  }`}
                />
              ))}
            </div>

            {renderStep()}

            <div className={styles["wizard-actions"]}>
              {step > 0 ? (
                <IconButton
                  text="Back"
                  bordered
                  onClick={() => setStep(step - 1)}
                />
              ) : (
                <div className={styles.spacer} />
              )}
              {step < totalSteps - 1 ? (
                <IconButton
                  text="Next"
                  bordered
                  onClick={() => setStep(step + 1)}
                />
              ) : (
                <IconButton
                  icon={<ConfirmIcon />}
                  text="Finish"
                  bordered
                  onClick={handleFinish}
                />
              )}
            </div>
          </div>
        </div>
      </div>
    </ErrorBoundary>
  );
}
