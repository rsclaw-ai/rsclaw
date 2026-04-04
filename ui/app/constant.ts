export const OWNER = "rsclaw-ai";
export const REPO = "rsclaw";
export const REPO_URL = `https://github.com/${OWNER}/${REPO}`;
export const ISSUE_URL = `https://github.com/${OWNER}/${REPO}/issues`;
export const UPDATE_URL = "https://app.rsclaw.ai/download";
export const RELEASE_URL = "https://app.rsclaw.ai/download";
export const FETCH_COMMIT_URL = `https://api.github.com/repos/${OWNER}/${REPO}/commits?per_page=1`;
export const FETCH_TAG_URL = `https://api.github.com/repos/${OWNER}/${REPO}/tags?per_page=1`;
export const RUNTIME_CONFIG_DOM = "danger-runtime-config";

// Gateway URL: set via NEXT_PUBLIC_RSCLAW_GATEWAY_URL env or default to 18888
export const RSCLAW_GATEWAY_URL =
  process.env.NEXT_PUBLIC_RSCLAW_GATEWAY_URL || "http://localhost:18888";

export const STABILITY_BASE_URL = "https://api.stability.ai";

export const OPENAI_BASE_URL = RSCLAW_GATEWAY_URL;
export const ANTHROPIC_BASE_URL = RSCLAW_GATEWAY_URL;

export const GEMINI_BASE_URL = RSCLAW_GATEWAY_URL;

export const BAIDU_BASE_URL = RSCLAW_GATEWAY_URL;
export const BAIDU_OATUH_URL = `${BAIDU_BASE_URL}/oauth/2.0/token`;

export const BYTEDANCE_BASE_URL = RSCLAW_GATEWAY_URL;

export const ALIBABA_BASE_URL = RSCLAW_GATEWAY_URL;

export const TENCENT_BASE_URL = RSCLAW_GATEWAY_URL;

export const MOONSHOT_BASE_URL = RSCLAW_GATEWAY_URL;
export const IFLYTEK_BASE_URL = RSCLAW_GATEWAY_URL;

export const DEEPSEEK_BASE_URL = RSCLAW_GATEWAY_URL;

export const XAI_BASE_URL = RSCLAW_GATEWAY_URL;

export const CHATGLM_BASE_URL = RSCLAW_GATEWAY_URL;

export const SILICONFLOW_BASE_URL = RSCLAW_GATEWAY_URL;

export const AI302_BASE_URL = RSCLAW_GATEWAY_URL;

export const CACHE_URL_PREFIX = "/api/cache";
export const UPLOAD_URL = `${CACHE_URL_PREFIX}/upload`;

export enum Path {
  Home = "/",
  Chat = "/chat",
  Settings = "/settings",
  Auth = "/auth",
  SearchChat = "/search-chat",
  GatewayControl = "/gateway",
  SetupWizard = "/setup",
  AgentManager = "/agents",
  ChannelConfig = "/channels",
  CronManager = "/cron",
  RsClawPanel = "/rsclaw",
  Onboarding = "/onboarding",
}

// RsClaw: all API paths route through the gateway directly.
// No Next.js proxy layer needed.
export enum ApiPath {
  Cors = "",
  Azure = "",
  OpenAI = "",
  Anthropic = "",
  Google = "",
  Baidu = "",
  ByteDance = "",
  Alibaba = "",
  Tencent = "",
  Moonshot = "",
  Iflytek = "",
  Stability = "",
  Artifacts = "/api/artifacts",
  XAI = "",
  ChatGLM = "",
  DeepSeek = "",
  SiliconFlow = "",
  "302.AI" = "",
}

export enum SlotID {
  AppBody = "app-body",
  CustomModel = "custom-model",
}

export enum FileName {
  Prompts = "prompts.json",
}

export enum StoreKey {
  Chat = "chat-next-web-store",
  Access = "access-control",
  Config = "app-config",
  Mask = "mask-store",
  Prompt = "prompt-store",
  Update = "chat-update",
}

export const DEFAULT_SIDEBAR_WIDTH = 300;
export const MAX_SIDEBAR_WIDTH = 500;
export const MIN_SIDEBAR_WIDTH = 230;
export const NARROW_SIDEBAR_WIDTH = 100;

export const ACCESS_CODE_PREFIX = "nk-";

export const LAST_INPUT_KEY = "last-input";
export const UNFINISHED_INPUT = (id: string) => "unfinished-input-" + id;

export const STORAGE_KEY = "rsclaw";

export const REQUEST_TIMEOUT_MS = 60000;
export const REQUEST_TIMEOUT_MS_FOR_THINKING = REQUEST_TIMEOUT_MS * 5;

export const EXPORT_MESSAGE_CLASS_NAME = "export-markdown";

export enum ServiceProvider {
  OpenAI = "OpenAI",
  Azure = "Azure",
  Google = "Google",
  Anthropic = "Anthropic",
  Baidu = "Baidu",
  ByteDance = "ByteDance",
  Alibaba = "Alibaba",
  Tencent = "Tencent",
  Moonshot = "Moonshot",
  Stability = "Stability",
  Iflytek = "Iflytek",
  XAI = "XAI",
  ChatGLM = "ChatGLM",
  DeepSeek = "DeepSeek",
  SiliconFlow = "SiliconFlow",
  "302.AI" = "302.AI",
}

// Google API safety settings, see https://ai.google.dev/gemini-api/docs/safety-settings
// BLOCK_NONE will not block any content, and BLOCK_ONLY_HIGH will block only high-risk content.
export enum GoogleSafetySettingsThreshold {
  BLOCK_NONE = "BLOCK_NONE",
  BLOCK_ONLY_HIGH = "BLOCK_ONLY_HIGH",
  BLOCK_MEDIUM_AND_ABOVE = "BLOCK_MEDIUM_AND_ABOVE",
  BLOCK_LOW_AND_ABOVE = "BLOCK_LOW_AND_ABOVE",
}

export enum ModelProvider {
  Stability = "Stability",
  GPT = "GPT",
  GeminiPro = "GeminiPro",
  Claude = "Claude",
  Ernie = "Ernie",
  Doubao = "Doubao",
  Qwen = "Qwen",
  Hunyuan = "Hunyuan",
  Moonshot = "Moonshot",
  Iflytek = "Iflytek",
  XAI = "XAI",
  ChatGLM = "ChatGLM",
  DeepSeek = "DeepSeek",
  SiliconFlow = "SiliconFlow",
  "302.AI" = "302.AI",
}

export const Stability = {
  GeneratePath: "v2beta/stable-image/generate",
  ExampleEndpoint: "https://api.stability.ai",
};

export const Anthropic = {
  ChatPath: "v1/messages",
  ChatPath1: "v1/complete",
  ExampleEndpoint: "https://api.anthropic.com",
  Vision: "2023-06-01",
};

export const OpenaiPath = {
  ChatPath: "v1/chat/completions",
  SpeechPath: "v1/audio/speech",
  ImagePath: "v1/images/generations",
  UsagePath: "dashboard/billing/usage",
  SubsPath: "dashboard/billing/subscription",
  ListModelPath: "v1/models",
};

export const Azure = {
  ChatPath: (deployName: string, apiVersion: string) =>
    `deployments/${deployName}/chat/completions?api-version=${apiVersion}`,
  // https://<your_resource_name>.openai.azure.com/openai/deployments/<your_deployment_name>/images/generations?api-version=<api_version>
  ImagePath: (deployName: string, apiVersion: string) =>
    `deployments/${deployName}/images/generations?api-version=${apiVersion}`,
  ExampleEndpoint: "https://{resource-url}/openai",
};

export const Google = {
  ExampleEndpoint: "https://generativelanguage.googleapis.com/",
  ChatPath: (modelName: string) =>
    `v1beta/models/${modelName}:streamGenerateContent`,
};

export const Baidu = {
  ExampleEndpoint: BAIDU_BASE_URL,
  ChatPath: (modelName: string) => {
    let endpoint = modelName;
    if (modelName === "ernie-4.0-8k") {
      endpoint = "completions_pro";
    }
    if (modelName === "ernie-4.0-8k-preview-0518") {
      endpoint = "completions_adv_pro";
    }
    if (modelName === "ernie-3.5-8k") {
      endpoint = "completions";
    }
    if (modelName === "ernie-speed-8k") {
      endpoint = "ernie_speed";
    }
    return `rpc/2.0/ai_custom/v1/wenxinworkshop/chat/${endpoint}`;
  },
};

export const ByteDance = {
  ExampleEndpoint: "https://ark.cn-beijing.volces.com/api/",
  ChatPath: "api/v3/chat/completions",
};

export const Alibaba = {
  ExampleEndpoint: ALIBABA_BASE_URL,
  ChatPath: (modelName: string) => {
    if (modelName.includes("vl") || modelName.includes("omni")) {
      return "v1/services/aigc/multimodal-generation/generation";
    }
    return `v1/services/aigc/text-generation/generation`;
  },
};

export const Tencent = {
  ExampleEndpoint: TENCENT_BASE_URL,
};

export const Moonshot = {
  ExampleEndpoint: MOONSHOT_BASE_URL,
  ChatPath: "v1/chat/completions",
};

export const Iflytek = {
  ExampleEndpoint: IFLYTEK_BASE_URL,
  ChatPath: "v1/chat/completions",
};

export const DeepSeek = {
  ExampleEndpoint: DEEPSEEK_BASE_URL,
  ChatPath: "chat/completions",
};

export const XAI = {
  ExampleEndpoint: XAI_BASE_URL,
  ChatPath: "v1/chat/completions",
};

export const ChatGLM = {
  ExampleEndpoint: CHATGLM_BASE_URL,
  ChatPath: "api/paas/v4/chat/completions",
  ImagePath: "api/paas/v4/images/generations",
  VideoPath: "api/paas/v4/videos/generations",
};

export const SiliconFlow = {
  ExampleEndpoint: SILICONFLOW_BASE_URL,
  ChatPath: "v1/chat/completions",
  ListModelPath: "v1/models?&sub_type=chat",
};

export const AI302 = {
  ExampleEndpoint: AI302_BASE_URL,
  ChatPath: "v1/chat/completions",
  EmbeddingsPath: "jina/v1/embeddings",
  ListModelPath: "v1/models?llm=1",
};

export const DEFAULT_INPUT_TEMPLATE = `{{input}}`; // input / time / model / lang
// export const DEFAULT_SYSTEM_TEMPLATE = `
// You are ChatGPT, a large language model trained by {{ServiceProvider}}.
// Knowledge cutoff: {{cutoff}}
// Current model: {{model}}
// Current time: {{time}}
// Latex inline: $x^2$
// Latex block: $$e=mc^2$$
// `;
export const DEFAULT_SYSTEM_TEMPLATE = `
You are a helpful AI assistant powered by RsClaw.
Current model: {{model}}
Current time: {{time}}
Latex inline: \\(x^2\\)
Latex block: $$e=mc^2$$
`;

export const SUMMARIZE_MODEL = "main";
export const GEMINI_SUMMARIZE_MODEL = "main";
export const DEEPSEEK_SUMMARIZE_MODEL = "main";

export const KnowledgeCutOffDate: Record<string, string> = {
  default: "2021-09",
  "gpt-4-turbo": "2023-12",
  "gpt-4-turbo-2024-04-09": "2023-12",
  "gpt-4-turbo-preview": "2023-12",
  "gpt-4.1": "2024-06",
  "gpt-4.1-2025-04-14": "2024-06",
  "gpt-4.1-mini": "2024-06",
  "gpt-4.1-mini-2025-04-14": "2024-06",
  "gpt-4.1-nano": "2024-06",
  "gpt-4.1-nano-2025-04-14": "2024-06",
  "gpt-4.5-preview": "2023-10",
  "gpt-4.5-preview-2025-02-27": "2023-10",
  "gpt-4o": "2023-10",
  "gpt-4o-2024-05-13": "2023-10",
  "gpt-4o-2024-08-06": "2023-10",
  "gpt-4o-2024-11-20": "2023-10",
  "chatgpt-4o-latest": "2023-10",
  "gpt-4o-mini": "2023-10",
  "gpt-4o-mini-2024-07-18": "2023-10",
  "gpt-4-vision-preview": "2023-04",
  "o1-mini-2024-09-12": "2023-10",
  "o1-mini": "2023-10",
  "o1-preview-2024-09-12": "2023-10",
  "o1-preview": "2023-10",
  "o1-2024-12-17": "2023-10",
  o1: "2023-10",
  "o3-mini-2025-01-31": "2023-10",
  "o3-mini": "2023-10",
  // After improvements,
  // it's now easier to add "KnowledgeCutOffDate" instead of stupid hardcoding it, as was done previously.
  "gemini-pro": "2023-12",
  "gemini-pro-vision": "2023-12",
  "deepseek-chat": "2024-07",
  "deepseek-coder": "2024-07",
};


export const VISION_MODEL_REGEXES = [
  /vision/,
  /gpt-4o/,
  /gpt-4\.1/,
  /claude.*[34]/,
  /gemini-1\.5/,
  /gemini-exp/,
  /gemini-2\.[05]/,
  /learnlm/,
  /qwen-vl/,
  /qwen2-vl/,
  /gpt-4-turbo(?!.*preview)/,
  /^dall-e-3$/,
  /glm-4v/,
  /vl/i,
  /o3/,
  /o4-mini/,
  /grok-4/i,
  /gpt-5/
];

export const EXCLUDE_VISION_MODEL_REGEXES = [/claude-3-5-haiku-20241022/];

const openaiModels = [
  // As of July 2024, gpt-4o-mini should be used in place of gpt-3.5-turbo,
  // as it is cheaper, more capable, multimodal, and just as fast. gpt-3.5-turbo is still available for use in the API.
  "gpt-3.5-turbo",
  "gpt-3.5-turbo-1106",
  "gpt-3.5-turbo-0125",
  "gpt-4",
  "gpt-4-0613",
  "gpt-4-32k",
  "gpt-4-32k-0613",
  "gpt-4-turbo",
  "gpt-4-turbo-preview",
  "gpt-4.1",
  "gpt-4.1-2025-04-14",
  "gpt-4.1-mini",
  "gpt-4.1-mini-2025-04-14",
  "gpt-4.1-nano",
  "gpt-4.1-nano-2025-04-14",
  "gpt-4.5-preview",
  "gpt-4.5-preview-2025-02-27",
  "gpt-5-chat",
  "gpt-5-mini",
  "gpt-5-nano",
  "gpt-5",
  "gpt-5-chat-2025-01-01-preview",
  "gpt-4o",
  "gpt-4o-2024-05-13",
  "gpt-4o-2024-08-06",
  "gpt-4o-2024-11-20",
  "chatgpt-4o-latest",
  "gpt-4o-mini",
  "gpt-4o-mini-2024-07-18",
  "gpt-4-vision-preview",
  "gpt-4-turbo-2024-04-09",
  "gpt-4-1106-preview",
  "dall-e-3",
  "o1-mini",
  "o1-preview",
  "o3-mini",
  "o3",
  "o4-mini",
];

const googleModels = [
  "gemini-1.5-pro-latest",
  "gemini-1.5-pro",
  "gemini-1.5-pro-002",
  "gemini-1.5-flash-latest",
  "gemini-1.5-flash-8b-latest",
  "gemini-1.5-flash",
  "gemini-1.5-flash-8b",
  "gemini-1.5-flash-002",
  "learnlm-1.5-pro-experimental",
  "gemini-exp-1206",
  "gemini-2.0-flash",
  "gemini-2.0-flash-exp",
  "gemini-2.0-flash-lite-preview-02-05",
  "gemini-2.0-flash-thinking-exp",
  "gemini-2.0-flash-thinking-exp-1219",
  "gemini-2.0-flash-thinking-exp-01-21",
  "gemini-2.0-pro-exp",
  "gemini-2.0-pro-exp-02-05",
  "gemini-2.5-pro-preview-06-05",
  "gemini-2.5-pro"
];

const anthropicModels = [
  "claude-instant-1.2",
  "claude-2.0",
  "claude-2.1",
  "claude-3-sonnet-20240229",
  "claude-3-opus-20240229",
  "claude-3-opus-latest",
  "claude-3-haiku-20240307",
  "claude-3-5-haiku-20241022",
  "claude-3-5-haiku-latest",
  "claude-3-5-sonnet-20240620",
  "claude-3-5-sonnet-20241022",
  "claude-3-5-sonnet-latest",
  "claude-3-7-sonnet-20250219",
  "claude-3-7-sonnet-latest",
  "claude-sonnet-4-20250514",
  "claude-opus-4-20250514",
];

const baiduModels = [
  "ernie-4.0-turbo-8k",
  "ernie-4.0-8k",
  "ernie-4.0-8k-preview",
  "ernie-4.0-8k-preview-0518",
  "ernie-4.0-8k-latest",
  "ernie-3.5-8k",
  "ernie-3.5-8k-0205",
  "ernie-speed-128k",
  "ernie-speed-8k",
  "ernie-lite-8k",
  "ernie-tiny-8k",
];

const bytedanceModels = [
  "Doubao-lite-4k",
  "Doubao-lite-32k",
  "Doubao-lite-128k",
  "Doubao-pro-4k",
  "Doubao-pro-32k",
  "Doubao-pro-128k",
];

const alibabaModes = [
  "qwen-turbo",
  "qwen-plus",
  "qwen-max",
  "qwen-max-0428",
  "qwen-max-0403",
  "qwen-max-0107",
  "qwen-max-longcontext",
  "qwen-omni-turbo",
  "qwen-vl-plus",
  "qwen-vl-max",
];

const tencentModels = [
  "hunyuan-pro",
  "hunyuan-standard",
  "hunyuan-lite",
  "hunyuan-role",
  "hunyuan-functioncall",
  "hunyuan-code",
  "hunyuan-vision",
];

const moonshotModels = [
  "moonshot-v1-auto",
  "moonshot-v1-8k",
  "moonshot-v1-32k",
  "moonshot-v1-128k",
  "moonshot-v1-8k-vision-preview",
  "moonshot-v1-32k-vision-preview",
  "moonshot-v1-128k-vision-preview",
  "kimi-thinking-preview",
  "kimi-k2-0711-preview",
  "kimi-latest",
];

const iflytekModels = [
  "general",
  "generalv3",
  "pro-128k",
  "generalv3.5",
  "4.0Ultra",
];

const deepseekModels = ["deepseek-chat", "deepseek-coder", "deepseek-reasoner"];

const xAIModes = [
  "grok-beta",
  "grok-2",
  "grok-2-1212",
  "grok-2-latest",
  "grok-vision-beta",
  "grok-2-vision-1212",
  "grok-2-vision",
  "grok-2-vision-latest",
  "grok-3-mini-fast-beta",
  "grok-3-mini-fast",
  "grok-3-mini-fast-latest",
  "grok-3-mini-beta",
  "grok-3-mini",
  "grok-3-mini-latest",
  "grok-3-fast-beta",
  "grok-3-fast",
  "grok-3-fast-latest",
  "grok-3-beta",
  "grok-3",
  "grok-3-latest",
  "grok-4",
  "grok-4-0709",
  "grok-4-fast-non-reasoning",
  "grok-4-fast-reasoning",
  "grok-code-fast-1",
];

const chatglmModels = [
  "glm-4-plus",
  "glm-4-0520",
  "glm-4",
  "glm-4-air",
  "glm-4-airx",
  "glm-4-long",
  "glm-4-flashx",
  "glm-4-flash",
  "glm-4v-plus",
  "glm-4v",
  "glm-4v-flash", // free
  "cogview-3-plus",
  "cogview-3",
  "cogview-3-flash", // free
  // 目前无法适配轮询任务
  //   "cogvideox",
  //   "cogvideox-flash", // free
];

const siliconflowModels = [
  "Qwen/Qwen2.5-7B-Instruct",
  "Qwen/Qwen2.5-72B-Instruct",
  "deepseek-ai/DeepSeek-R1",
  "deepseek-ai/DeepSeek-R1-Distill-Llama-70B",
  "deepseek-ai/DeepSeek-R1-Distill-Llama-8B",
  "deepseek-ai/DeepSeek-R1-Distill-Qwen-1.5B",
  "deepseek-ai/DeepSeek-R1-Distill-Qwen-14B",
  "deepseek-ai/DeepSeek-R1-Distill-Qwen-32B",
  "deepseek-ai/DeepSeek-R1-Distill-Qwen-7B",
  "deepseek-ai/DeepSeek-V3",
  "meta-llama/Llama-3.3-70B-Instruct",
  "THUDM/glm-4-9b-chat",
  "Pro/deepseek-ai/DeepSeek-R1",
  "Pro/deepseek-ai/DeepSeek-V3",
];

const ai302Models = [
  "deepseek-chat",
  "gpt-4o",
  "chatgpt-4o-latest",
  "llama3.3-70b",
  "deepseek-reasoner",
  "gemini-2.0-flash",
  "claude-3-7-sonnet-20250219",
  "claude-3-7-sonnet-latest",
  "grok-3-beta",
  "grok-3-mini-beta",
  "gpt-4.1",
  "gpt-4.1-mini",
  "o3",
  "o4-mini",
  "qwen3-235b-a22b",
  "qwen3-32b",
  "gemini-2.5-pro-preview-05-06",
  "llama-4-maverick",
  "gemini-2.5-flash",
  "claude-sonnet-4-20250514",
  "claude-opus-4-20250514",
  "gemini-2.5-pro",
];

let seq = 1000; // built-in model sequence starts at 1000
export const DEFAULT_MODELS = [
  {
    name: "main",
    available: true,
    sorted: seq++,
    provider: {
      id: "openai",
      providerName: "OpenAI",
      providerType: "openai",
      sorted: 1,
    },
  },
] as const;

export const CHAT_PAGE_SIZE = 15;
export const MAX_RENDER_MSG_COUNT = 45;


export const DEFAULT_GA_ID = "G-89WN60ZK2E";

export const SAAS_CHAT_URL = "https://rsclaw.ai";
export const SAAS_CHAT_UTM_URL = "https://rsclaw.ai";
