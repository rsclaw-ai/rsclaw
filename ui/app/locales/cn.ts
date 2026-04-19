import { getClientConfig } from "../config/client";
import { SubmitKey } from "../store/config";
import { SAAS_CHAT_UTM_URL } from "@/app/constant";

const isApp = !!getClientConfig()?.isApp;

const cn = {
  WIP: "该功能仍在开发中……",
  Error: {
    Unauthorized: `获取访问密钥失败，请检查配置文件 rsclaw.json5 或到[设置](/#/settings)重新运行引导程序。`,
  },
  Auth: {
    Return: "返回",
    Title: "需要密码",
    Tips: "管理员开启了密码验证，请在下方填入访问码",
    SubTips: "或者输入你的 OpenAI 或 Google AI 密钥",
    Input: "在此处填写访问码",
    Confirm: "确认",
    Later: "稍后再说",
    SaasTips: "配置太麻烦，想要立即使用",
    TopTips:
      "RsClaw 螃蟹AI自动化管家 - 通过统一端点访问所有模型",
  },
  ChatItem: {
    ChatItemCount: (count: number) => `${count} 条对话`,
  },
  Chat: {
    SubTitle: (count: number) => `共 ${count} 条对话`,
    EditMessage: {
      Title: "编辑消息记录",
      Topic: {
        Title: "聊天主题",
        SubTitle: "更改当前聊天主题",
      },
    },
    Actions: {
      ChatList: "查看消息列表",
      CompressedHistory: "查看压缩后的历史 Prompt",
      Export: "导出聊天记录",
      Copy: "复制",
      Stop: "停止",
      Retry: "重试",
      Pin: "固定",
      PinToastContent: "已将 1 条对话固定至预设提示词",
      PinToastAction: "查看",
      Delete: "删除",
      Edit: "编辑",
      FullScreen: "全屏",
      RefreshTitle: "刷新标题",
      RefreshToast: "已发送刷新标题请求",
      Speech: "朗读",
      StopSpeech: "停止",
    },
    Commands: {
      new: "新建聊天",
      newm: "从面具新建聊天",
      next: "下一个聊天",
      prev: "上一个聊天",
      clear: "清除上下文",
      fork: "复制聊天",
      del: "删除聊天",
    },
    InputActions: {
      Stop: "停止响应",
      ToBottom: "滚到最新",
      Theme: {
        auto: "自动主题",
        light: "亮色模式",
        dark: "深色模式",
      },
      Prompt: "快捷指令",
      Masks: "所有面具",
      Clear: "清除聊天",
      Settings: "对话设置",
      UploadImage: "上传图片",
    },
    Rename: "重命名对话",
    Typing: "正在输入…",
    Input: (submitKey: string) => {
      var inputHints = `${submitKey} 发送`;
      if (submitKey === String(SubmitKey.Enter)) {
        inputHints += "，Shift + Enter 换行";
      }
      return inputHints + "，/ 触发命令";
    },
    Send: "发送",
    StartSpeak: "说话",
    StopSpeak: "停止",
    Config: {
      Reset: "清除记忆",
      SaveAs: "存为面具",
      Topic: {
        Title: "对话主题",
        SubTitle: "设置当前对话的主题",
      },
    },
    IsContext: "预设提示词",
    ShortcutKey: {
      Title: "键盘快捷方式",
      newChat: "打开新聊天",
      focusInput: "聚焦输入框",
      copyLastMessage: "复制最后一个回复",
      copyLastCode: "复制最后一个代码块",
      showShortcutKey: "显示快捷方式",
      clearContext: "清除上下文",
    },
  },
  Export: {
    Title: "分享聊天记录",
    Copy: "全部复制",
    Download: "下载文件",
    Share: "分享到 ShareGPT",
    MessageFromYou: "用户",
    MessageFromChatGPT: "AI",
    Format: {
      Title: "导出格式",
      SubTitle: "可以导出 Markdown 文本或者 PNG 图片",
    },
    IncludeContext: {
      Title: "包含面具上下文",
      SubTitle: "是否在消息中展示面具上下文",
    },
    Steps: {
      Select: "选取",
      Preview: "预览",
    },
    Image: {
      Toast: "正在生成截图",
      Modal: "长按或右键保存图片",
    },
    Artifacts: {
      Title: "分享页面",
      Error: "分享失败",
    },
  },
  Select: {
    Search: "搜索消息",
    All: "选取全部",
    Latest: "最近几条",
    Clear: "清除选中",
  },
  Memory: {
    Title: "历史摘要",
    EmptyContent: "对话内容过短，无需总结",
    Send: "自动压缩聊天记录并作为上下文发送",
    Copy: "复制摘要",
    Reset: "[unused]",
    ResetConfirm: "确认清空历史摘要？",
  },
  Home: {
    NewChat: "新建会话",
    DeleteChat: "确认删除选中的对话？",
    RenameChat: "重命名",
    RenamePlaceholder: "输入新名称...",
    DeleteToast: "已删除会话",
    Revert: "撤销",
  },
  Settings: {
    Title: "设置",
    SubTitle: "所有设置选项",
    ShowPassword: "显示密码",

    Danger: {
      Reset: {
        Title: "重置所有设置",
        SubTitle: "重置所有设置项回默认值",
        Action: "立即重置",
        Confirm: "确认重置所有设置？",
      },
      Clear: {
        Title: "清除所有数据",
        SubTitle: "清除所有聊天、设置数据",
        Action: "立即清除",
        Confirm: "确认清除所有聊天、设置数据？",
      },
    },
    Lang: {
      Name: "语言",
      All: "所有语言",
    },
    Avatar: "头像",
    FontSize: {
      Title: "字体大小",
      SubTitle: "聊天内容的字体大小",
    },
    FontFamily: {
      Title: "聊天字体",
      SubTitle: "聊天内容的字体，若置空则应用全局默认字体",
      Placeholder: "字体名称",
    },
    InjectSystemPrompts: {
      Title: "注入系统级提示信息",
      SubTitle: "强制给每次请求的消息列表开头添加一个模拟 ChatGPT 的系统提示",
    },
    InputTemplate: {
      Title: "用户输入预处理",
      SubTitle: "用户最新的一条消息会填充到此模板",
    },

    Update: {
      Version: (x: string) => `当前版本：${x}`,
      IsLatest: "已是最新版本",
      CheckUpdate: "检查更新",
      IsChecking: "正在检查更新...",
      FoundUpdate: (x: string) => `发现新版本：${x}`,
      GoToUpdate: "前往更新",
      Success: "更新成功！",
      Failed: "更新失败",
    },
    SendKey: "发送键",
    Theme: "主题",
    TightBorder: "无边框模式",
    SendPreviewBubble: {
      Title: "预览气泡",
      SubTitle: "在预览气泡中预览 Markdown 内容",
    },
    AutoGenerateTitle: {
      Title: "自动生成标题",
      SubTitle: "根据对话内容生成合适的标题",
    },
    Sync: {
      CloudState: "云端数据",
      NotSyncYet: "还没有进行过同步",
      Success: "同步成功",
      Fail: "同步失败",

      Config: {
        Modal: {
          Title: "配置云同步",
          Check: "检查可用性",
        },
        SyncType: {
          Title: "同步类型",
          SubTitle: "选择喜爱的同步服务器",
        },
        Proxy: {
          Title: "启用代理",
          SubTitle: "在浏览器中同步时，必须启用代理以避免跨域限制",
        },
        ProxyUrl: {
          Title: "代理地址",
          SubTitle: "仅适用于本项目自带的跨域代理",
        },

        WebDav: {
          Endpoint: "WebDAV 地址",
          UserName: "用户名",
          Password: "密码",
        },

        UpStash: {
          Endpoint: "UpStash Redis REST Url",
          UserName: "备份名称",
          Password: "UpStash Redis REST Token",
        },
      },

      LocalState: "本地数据",
      Overview: (overview: any) => {
        return `${overview.chat} 次对话，${overview.message} 条消息，${overview.prompt} 条提示词，${overview.mask} 个面具`;
      },
      ImportFailed: "导入失败",
    },
    Mask: {
      Splash: {
        Title: "面具启动页",
        SubTitle: "新建聊天时，展示面具启动页",
      },
      Builtin: {
        Title: "隐藏内置面具",
        SubTitle: "在所有面具列表中隐藏内置面具",
      },
    },
    Prompt: {
      Disable: {
        Title: "禁用提示词自动补全",
        SubTitle: "在输入框开头输入 / 即可触发自动补全",
      },
      List: "自定义提示词列表",
      ListCount: (builtin: number, custom: number) =>
        `内置 ${builtin} 条，用户定义 ${custom} 条`,
      Edit: "编辑",
      Modal: {
        Title: "提示词列表",
        Add: "新建",
        Search: "搜索提示词",
      },
      EditModal: {
        Title: "编辑提示词",
      },
    },
    HistoryCount: {
      Title: "附带历史消息数",
      SubTitle: "每次请求携带的历史消息数",
    },
    CompressThreshold: {
      Title: "历史消息长度压缩阈值",
      SubTitle: "当未压缩的历史消息超过该值时，将进行压缩",
    },

    Usage: {
      Title: "余额查询",
      SubTitle(used: any, total: any) {
        return `本月已使用 $${used}，订阅总额 $${total}`;
      },
      IsChecking: "正在检查…",
      Check: "重新检查",
      NoAccess: "输入 API Key 或访问密码查看余额",
    },

    Access: {
      SaasStart: {
        Title: "使用 RsClaw AI",
        Label: "",
        SubTitle:
          "RsClaw 螃蟹AI自动化管家 - 统一访问所有模型",
        ChatNow: "立刻对话",
      },
      AccessCode: {
        Title: "访问密码",
        SubTitle: "管理员已开启加密访问",
        Placeholder: "请输入访问密码",
      },
      CustomEndpoint: {
        Title: "自定义接口",
        SubTitle: "是否使用自定义 Azure 或 OpenAI 服务",
      },
      Provider: {
        Title: "模型服务商",
        SubTitle: "切换不同的服务商",
      },
      OpenAI: {
        ApiKey: {
          Title: "API Key",
          SubTitle: "使用自定义 OpenAI Key 绕过密码访问限制",
          Placeholder: "OpenAI API Key",
        },

        Endpoint: {
          Title: "接口地址",
          SubTitle: "除默认地址外，必须包含 http(s)://",
        },
      },
      Azure: {
        ApiKey: {
          Title: "接口密钥",
          SubTitle: "使用自定义 Azure Key 绕过密码访问限制",
          Placeholder: "Azure API Key",
        },

        Endpoint: {
          Title: "接口地址",
          SubTitle: "样例：",
        },

        ApiVerion: {
          Title: "接口版本 (azure api version)",
          SubTitle: "选择指定的部分版本",
        },
      },
      Anthropic: {
        ApiKey: {
          Title: "接口密钥",
          SubTitle: "使用自定义 Anthropic Key 绕过密码访问限制",
          Placeholder: "Anthropic API Key",
        },

        Endpoint: {
          Title: "接口地址",
          SubTitle: "样例：",
        },

        ApiVerion: {
          Title: "接口版本 (claude api version)",
          SubTitle: "选择一个特定的 API 版本输入",
        },
      },
      Google: {
        ApiKey: {
          Title: "API 密钥",
          SubTitle: "从 Google AI 获取您的 API 密钥",
          Placeholder: "Google AI API KEY",
        },

        Endpoint: {
          Title: "终端地址",
          SubTitle: "示例：",
        },

        ApiVersion: {
          Title: "API 版本（仅适用于 gemini-pro）",
          SubTitle: "选择一个特定的 API 版本",
        },
        GoogleSafetySettings: {
          Title: "Google 安全过滤级别",
          SubTitle: "设置内容过滤级别",
        },
      },
      Baidu: {
        ApiKey: {
          Title: "API Key",
          SubTitle: "使用自定义 Baidu API Key",
          Placeholder: "Baidu API Key",
        },
        SecretKey: {
          Title: "Secret Key",
          SubTitle: "使用自定义 Baidu Secret Key",
          Placeholder: "Baidu Secret Key",
        },
        Endpoint: {
          Title: "接口地址",
          SubTitle: "不支持自定义前往.env配置",
        },
      },
      Tencent: {
        ApiKey: {
          Title: "API Key",
          SubTitle: "使用自定义腾讯云API Key",
          Placeholder: "Tencent API Key",
        },
        SecretKey: {
          Title: "Secret Key",
          SubTitle: "使用自定义腾讯云Secret Key",
          Placeholder: "Tencent Secret Key",
        },
        Endpoint: {
          Title: "接口地址",
          SubTitle: "不支持自定义前往.env配置",
        },
      },
      ByteDance: {
        ApiKey: {
          Title: "接口密钥",
          SubTitle: "使用自定义 ByteDance API Key",
          Placeholder: "ByteDance API Key",
        },
        Endpoint: {
          Title: "接口地址",
          SubTitle: "样例：",
        },
      },
      Alibaba: {
        ApiKey: {
          Title: "接口密钥",
          SubTitle: "使用自定义阿里云API Key",
          Placeholder: "Alibaba Cloud API Key",
        },
        Endpoint: {
          Title: "接口地址",
          SubTitle: "样例：",
        },
      },
      Moonshot: {
        ApiKey: {
          Title: "接口密钥",
          SubTitle: "使用自定义月之暗面API Key",
          Placeholder: "Moonshot API Key",
        },
        Endpoint: {
          Title: "接口地址",
          SubTitle: "样例：",
        },
      },
      DeepSeek: {
        ApiKey: {
          Title: "接口密钥",
          SubTitle: "使用自定义DeepSeek API Key",
          Placeholder: "DeepSeek API Key",
        },
        Endpoint: {
          Title: "接口地址",
          SubTitle: "样例：",
        },
      },
      XAI: {
        ApiKey: {
          Title: "接口密钥",
          SubTitle: "使用自定义XAI API Key",
          Placeholder: "XAI API Key",
        },
        Endpoint: {
          Title: "接口地址",
          SubTitle: "样例：",
        },
      },
      ChatGLM: {
        ApiKey: {
          Title: "接口密钥",
          SubTitle: "使用自定义 ChatGLM API Key",
          Placeholder: "ChatGLM API Key",
        },
        Endpoint: {
          Title: "接口地址",
          SubTitle: "样例：",
        },
      },
      SiliconFlow: {
        ApiKey: {
          Title: "接口密钥",
          SubTitle: "使用自定义硅基流动 API Key",
          Placeholder: "硅基流动 API Key",
        },
        Endpoint: {
          Title: "接口地址",
          SubTitle: "样例：",
        },
      },
      Stability: {
        ApiKey: {
          Title: "接口密钥",
          SubTitle: "使用自定义 Stability API Key",
          Placeholder: "Stability API Key",
        },
        Endpoint: {
          Title: "接口地址",
          SubTitle: "样例：",
        },
      },
      Iflytek: {
        ApiKey: {
          Title: "ApiKey",
          SubTitle: "从讯飞星火控制台获取的 APIKey",
          Placeholder: "APIKey",
        },
        ApiSecret: {
          Title: "ApiSecret",
          SubTitle: "从讯飞星火控制台获取的 APISecret",
          Placeholder: "APISecret",
        },
        Endpoint: {
          Title: "接口地址",
          SubTitle: "样例：",
        },
      },
      CustomModel: {
        Title: "自定义模型名",
        SubTitle: "增加自定义模型可选项，使用英文逗号隔开",
      },
      AI302: {
        ApiKey: {
          Title: "接口密钥",
          SubTitle: "使用自定义302.AI API Key",
          Placeholder: "302.AI API Key",
        },
        Endpoint: {
          Title: "接口地址",
          SubTitle: "样例：",
        },
      },
    },

    Model: "模型 (model)",
    CompressModel: {
      Title: "对话摘要模型",
      SubTitle: "用于压缩历史记录、生成对话标题的模型",
    },
    VideoModel: {
      Title: "视频生成模型",
      SubTitle: "用于文生视频的模型 (Seedance / MiniMax / Kling)",
    },
    BtwTokens: {
      Title: "旁路上下文预算",
      SubTitle: "/ctx 旁路查询的最大 token 数",
    },
    KvCacheMode: {
      Title: "KV Cache 模式",
      SubTitle: "0=关闭 1=完整追加(默认) 2=增量",
    },
    Temperature: {
      Title: "随机性 (temperature)",
      SubTitle: "值越大，回复越随机",
    },
    TopP: {
      Title: "核采样 (top_p)",
      SubTitle: "与随机性类似，但不要和随机性一起更改",
    },
    MaxTokens: {
      Title: "单次回复限制 (max_tokens)",
      SubTitle: "单次交互所用的最大 Token 数",
    },
    PresencePenalty: {
      Title: "话题新鲜度 (presence_penalty)",
      SubTitle: "值越大，越有可能扩展到新话题",
    },
    FrequencyPenalty: {
      Title: "频率惩罚度 (frequency_penalty)",
      SubTitle: "值越大，越有可能降低重复字词",
    },
    TTS: {
      Enable: {
        Title: "启用文本转语音",
        SubTitle: "启用文本生成语音服务",
      },
      Autoplay: {
        Title: "启用自动朗读",
        SubTitle: "自动生成语音并播放，需先开启文本转语音开关",
      },
      Model: "模型",
      Engine: "转换引擎",
      Voice: {
        Title: "声音",
        SubTitle: "生成语音时使用的声音",
      },
      Speed: {
        Title: "速度",
        SubTitle: "生成语音的速度",
      },
    },
    Realtime: {
      Enable: {
        Title: "实时聊天",
        SubTitle: "开启实时聊天功能",
      },
      Provider: {
        Title: "模型服务商",
        SubTitle: "切换不同的服务商",
      },
      Model: {
        Title: "模型",
        SubTitle: "选择一个模型",
      },
      ApiKey: {
        Title: "API Key",
        SubTitle: "API Key",
        Placeholder: "API Key",
      },
      Azure: {
        Endpoint: {
          Title: "接口地址",
          SubTitle: "接口地址",
        },
        Deployment: {
          Title: "部署名称",
          SubTitle: "部署名称",
        },
      },
      Temperature: {
        Title: "随机性 (temperature)",
        SubTitle: "值越大，回复越随机",
      },
    },
  },
  Store: {
    DefaultTopic: "新的会话",
    BotHello: "",
    Error: "出错了，稍后重试吧",
    Prompt: {
      History: (content: string) => "这是历史聊天总结作为前情提要：" + content,
      Topic:
        "使用四到五个字直接返回这句话的简要主题，不要解释、不要标点、不要语气词、不要多余文本，不要加粗，如果没有主题，请直接返回“闲聊”",
      Summarize:
        "简要总结一下对话内容，用作后续的上下文提示 prompt，控制在 200 字以内",
    },
  },
  Copy: {
    Success: "已写入剪贴板",
    Failed: "复制失败，请赋予剪贴板权限",
  },
  Download: {
    Success: "内容已下载到您的目录。",
    Failed: "下载失败。",
  },
  Context: {
    Toast: (x: any) => `包含 ${x} 条预设提示词`,
    Edit: "当前对话设置",
    Add: "新增一条对话",
    Clear: "上下文已清除",
    Revert: "恢复上下文",
  },
  Discovery: {
    Name: "发现",
  },
  Mcp: {
    Name: "MCP",
  },
  FineTuned: {
    Sysmessage: "你是一个助手",
  },
  SearchChat: {
    Name: "搜索聊天记录",
    Page: {
      Title: "搜索聊天记录",
      Search: "输入搜索关键词",
      NoResult: "没有找到结果",
      NoData: "没有数据",
      Loading: "加载中",

      SubTitle: (count: number) => `搜索到 ${count} 条结果`,
    },
    Item: {
      View: "查看",
    },
  },
  Plugin: {
    Name: "插件",
    Page: {
      Title: "插件",
      SubTitle: (count: number) => `${count} 个插件`,
      Search: "搜索插件",
      Create: "新建",
      Find: "您可以在Github上找到优秀的插件：",
    },
    Item: {
      Info: (count: number) => `${count} 方法`,
      View: "查看",
      Edit: "编辑",
      Delete: "删除",
      DeleteConfirm: "确认删除？",
    },
    Auth: {
      None: "不需要授权",
      Basic: "Basic",
      Bearer: "Bearer",
      Custom: "自定义",
      CustomHeader: "自定义参数名称",
      Token: "Token",
      Proxy: "使用代理",
      ProxyDescription: "使用代理解决 CORS 错误",
      Location: "位置",
      LocationHeader: "Header",
      LocationQuery: "Query",
      LocationBody: "Body",
    },
    EditModal: {
      Title: (readonly: boolean) => `编辑插件 ${readonly ? "（只读）" : ""}`,
      Download: "下载",
      Auth: "授权方式",
      Content: "OpenAPI Schema",
      Load: "从网页加载",
      Method: "方法",
      Error: "格式错误",
    },
  },
  Mask: {
    Name: "面具",
    Page: {
      Title: "预设角色面具",
      SubTitle: (count: number) => `${count} 个预设角色定义`,
      Search: "搜索角色面具",
      Create: "新建",
    },
    Item: {
      Info: (count: number) => `包含 ${count} 条预设对话`,
      Chat: "对话",
      View: "查看",
      Edit: "编辑",
      Delete: "删除",
      DeleteConfirm: "确认删除？",
    },
    EditModal: {
      Title: (readonly: boolean) =>
        `编辑预设面具 ${readonly ? "（只读）" : ""}`,
      Download: "下载预设",
      Clone: "克隆预设",
    },
    Config: {
      Avatar: "角色头像",
      Name: "角色名称",
      Sync: {
        Title: "使用全局设置",
        SubTitle: "当前对话是否使用全局模型设置",
        Confirm: "当前对话的自定义设置将会被自动覆盖，确认启用全局设置？",
      },
      HideContext: {
        Title: "隐藏预设对话",
        SubTitle: "隐藏后预设对话不会出现在聊天界面",
      },
      Artifacts: {
        Title: "启用Artifacts",
        SubTitle: "启用之后可以直接渲染HTML页面",
      },
      CodeFold: {
        Title: "启用代码折叠",
        SubTitle: "启用之后可以自动折叠/展开过长的代码块",
      },
      Share: {
        Title: "分享此面具",
        SubTitle: "生成此面具的直达链接",
        Action: "复制链接",
      },
    },
  },
  NewChat: {
    Return: "返回",
    Skip: "直接开始",
    NotShow: "不再展示",
    ConfirmNoShow: "确认禁用？禁用后可以随时在设置中重新启用。",
    Title: "挑选一个面具",
    SubTitle: "现在开始，与面具背后的灵魂思维碰撞",
    More: "查看全部",
  },

  URLCommand: {
    Code: "检测到链接中已经包含访问码，是否自动填入？",
    Settings: "检测到链接中包含了预制设置，是否自动填入？",
  },

  UI: {
    Confirm: "确认",
    Cancel: "取消",
    Close: "关闭",
    Create: "新建",
    Edit: "编辑",
    Export: "导出",
    Import: "导入",
    Sync: "同步",
    Config: "配置",
  },
  Exporter: {
    Description: {
      Title: "只有清除上下文之后的消息会被展示",
    },
    Model: "模型",
    Messages: "消息",
    Topic: "主题",
    Time: "时间",
  },
  SdPanel: {
    Prompt: "画面提示",
    NegativePrompt: "否定提示",
    PleaseInput: (name: string) => `请输入${name}`,
    AspectRatio: "横纵比",
    ImageStyle: "图像风格",
    OutFormat: "输出格式",
    AIModel: "AI模型",
    ModelVersion: "模型版本",
    Submit: "提交生成",
    ParamIsRequired: (name: string) => `${name}不能为空`,
    Styles: {
      D3Model: "3D模型",
      AnalogFilm: "模拟电影",
      Anime: "动漫",
      Cinematic: "电影风格",
      ComicBook: "漫画书",
      DigitalArt: "数字艺术",
      Enhance: "增强",
      FantasyArt: "幻想艺术",
      Isometric: "等角",
      LineArt: "线描",
      LowPoly: "低多边形",
      ModelingCompound: "建模材料",
      NeonPunk: "霓虹朋克",
      Origami: "折纸",
      Photographic: "摄影",
      PixelArt: "像素艺术",
      TileTexture: "贴图",
    },
  },
  Sd: {
    SubTitle: (count: number) => `共 ${count} 条绘画`,
    Actions: {
      Params: "查看参数",
      Copy: "复制提示词",
      Delete: "删除",
      Retry: "重试",
      ReturnHome: "返回首页",
      History: "查看历史",
    },
    EmptyRecord: "暂无绘画记录",
    Status: {
      Name: "状态",
      Success: "成功",
      Error: "失败",
      Wait: "等待中",
      Running: "运行中",
    },
    Danger: {
      Delete: "确认删除？",
    },
    GenerateParams: "生成参数",
    Detail: "详情",
  },

  NewChatDialog: {
    Title: "新建会话",
    SessionName: "会话名称",
    SessionNamePlaceholder: "输入会话名称...",
    Agent: "智能体",
    AgentDefault: "默认",
    Create: "创建",
    Cancel: "取消",
  },

  RsClawSettings: {
    GatewayUrl: "网关地址",
    GatewayUrlSub: "RsClaw 网关端点",
    AuthToken: "认证令牌",
    AuthTokenSub: "网关认证 Bearer Token",
    Agent: "智能体",
    AgentSub: "选择用于聊天的智能体",
    AgentDefault: "默认",
    AgentLoading: "加载中...",
    AgentLoadFailed: "无法获取智能体列表",
    AutoStart: "开机启动",
    AutoStartSub: "登录时自动启动 RsClaw",
  },

  GatewayControl: {
    Title: "网关控制",
    SubTitle: "管理你的 rsclaw 网关",
    Checking: "检测中...",
    Running: "网关运行中",
    Stopped: "网关已停止",
    NotResponding: "网关未响应",
    Start: "启动",
    Stop: "停止",
    Restart: "重启",
    Version: "版本",
    Port: "端口",
    Agents: "智能体",
    StatusLabel: "状态",
    Online: "在线",
    Offline: "离线",
    ActiveAgents: "活跃智能体",
    NoAgents: "未配置智能体",
    StartToSee: "启动网关以查看智能体",
  },

  RsClawPanel: {
    Splash: "欢迎使用螃蟹AI自动化管家!",
    Title: "RsClaw 控制台",
    BackToChat: "返回会话",
    Running: "运行中",
    Offline: "离线",

    SubTitle: "螃蟹AI自动化管家",
    Sidebar: {
      Service: "网关状态",
      ServiceTitle: "网关状态",
      Config: "配置管理",
      ConfigTitle: "配置管理",
      Agents: "多智能体",
      AgentsTitle: "多智能体管理",
    },

    Nav: {
      Status: "状态",
      GatewayStatus: "网关状态",
      Config: "配置",
      ConfigEditor: "配置管理",
      AgentManager: "智能体管理",
      GettingStarted: "快速开始",
      SetupWizard: "上手向导",
      New: "新",
    },

    Status: {
      PageTitle: "网关状态",
      GatewayName: "rsclaw gateway",
      NotResponding: "网关未响应",
      Uptime: "运行时间",
      Restart: "重启",
      Stop: "停止",
      Start: "启动",
      Cancel: "取消",
      StopTitle: "停止网关",
      StopSub: "网关停止后，所有消息通道将断开连接，进行中的会话也将终止。",
      StopNote: "停止后用户将无法通过任何通道发送消息，直到手动重新启动网关。",
      StopConfirm: "确认停止",
      Stopping: "停止中",
      RestartTitle: "重启网关",
      RestartSub: "网关将短暂中断后自动恢复，通道连接和配置会自动重新加载。",
      RestartNote: "重启期间（约 2-5 秒）所有通道暂时不可用，重启后自动恢复。",
      RestartConfirm: "确认重启",
      Restarting: "重启中",
      StartTitle: "启动网关",
      StartSub: "启动网关后将自动连接所有已配置的消息通道。",
      StartNote: "首次启动可能需要几秒钟来初始化通道连接。",
      StartConfirm: "确认启动",
      Starting: "启动中",
      CurrentStatus: "当前状态",
      ChannelCount: "已连接通道",
      SessionCount: "活跃会话",
      UptimeLabel: "运行时长",
      Unit: "个",
      ConnectedChannels: "已连接通道",
      ActiveSessions: "活跃会话",
      Last24h: "近 24 小时",
      Memory: "内存占用",
      SingleProcess: "Rust 单进程",
      None: "无",
      MessageChannels: "消息通道",
      NoChannels: "未配置通道",
      StartGateway: "启动网关以查看通道",
      RealtimeLogs: "实时日志",
      Pause: "暂停",
      Resume: "继续",
      Clear: "清空",
      WaitingLogs: "等待日志数据...",
      GatewayNotRunning: "网关未运行",
      Connected: "已连接",
      Disconnected: "未启用",
      Error: "错误",
    },

    Config: {
      PageTitle: "配置管理",
      PageSub: "~/.rsclaw/rsclaw.json5 -- 实时预览",
      Reset: "重置",
      SaveAndReload: "保存并热重载",
      Saving: "保存中...",
      Loading: "加载配置中...",
      SaveSuccess: "配置已保存并重载",
      SaveFailed: "保存失败: ",
      Gateway: "网关 (gateway)",
      Port: "端口 (port)",
      Bind: "绑定地址 (bind)",
      BindLoopback: "loopback（仅本机）",
      BindAll: "0.0.0.0（所有接口）",
      BindCustom: "自定义",
      Language: "网关语言 (language)",
      ProcessingTimeout: "处理超时 (秒)",
      Models: "模型 (models)",
      Channels: "通道 (channels)",
      Tools: "功能开关 (tools)",
      LivePreview: "实时预览",
      Copy: "复制",
      SaveAndApply: "保存并应用",
      ReloadNote: "保存后会自动触发热重载。注意：修改智能体、通道等核心配置需要重启网关才能生效。",
    },

    Agents: {
      PageTitle: "智能体管理",
      PageSub: "agents.list -- 配置每个智能体的模型和通道绑定",
      NewAgent: "+ 新建智能体",
      AgentNote: "agents.defaults 的压缩和模型设置适用于所有智能体；支持单独覆盖。",
      NoAgents: '暂无智能体配置。点击 "+ 新建智能体" 添加。',
      Edit: "编辑",
      Delete: "删除",
      ConfirmDelete: (id: string) => `确认删除智能体 "${id}"?`,
      ChannelsLabel: "通道",
      ToolsLabel: "工具",
      NoneValue: "无",
      AddAgent: "添加智能体",
      EditAgent: "编辑智能体",
      AgentId: "智能体 ID",
      AgentName: "名称（可选）",
      AgentNamePlaceholder: "显示名称，如：销售助手",
      Avatar: "头像",
      AvatarHint: "点击选择 emoji 头像",
      Model: "模型",
      ChannelsInput: "通道（逗号分隔）",
      ToolsetInput: "工具集（逗号分隔）",
      Cancel: "取消",
      Create: "创建",
      Update: "更新",
      SaveFailed: "保存智能体失败: ",
      Workspace: "Workspace",
      BackToList: "返回列表",
      Templates: "模板库",
      UseTemplate: "使用模板",
      TemplateApplied: "模板已应用到 workspace",
      DeleteFailed: "删除智能体失败: ",
    },

    Workspace: {
      PageTitle: "Workspace 文件",
      PageSub: "编辑智能体的系统提示词和配置文件",
      DefaultAgent: "默认智能体",
      Save: "保存",
      SaveSuccess: "文件已保存",
      SaveFailed: "保存失败: ",
      NewFile: "新建",
      NewFileName: "文件名",
      NewFilePlaceholder: "CUSTOM.md",
      Create: "创建",
      Cancel: "取消",
      EditorPlaceholder: "选择左侧文件开始编辑...",
      NavTitle: "Workspace",
    },

    Doctor: {
      PageTitle: "安全检查",
      PageSub: "诊断配置问题并自动修复",
      NavTitle: "安全检查",
      RunCheck: "检查",
      RunFix: "修复",
      Running: "检查中...",
      Fixing: "修复中...",
      NoIssues: "所有检查通过，无问题。",
      NotRun: '点击 "检查" 按钮开始诊断。',
    },

    Wizard: {
      PageTitle: "首次上手向导",
      PageSub: "5 步完成 rsclaw 的安装与配置，无需命令行",
      Next: "下一步",
      Back: "上一步",

      DetectTitle: "检测环境",
      DetectSub: "检查是否有现有的 OpenClaw 安装需要迁移。",
      DetectChecking: "检测中...",
      DetectFound: "检测到 OpenClaw 安装",
      DetectNotFound: "未检测到 OpenClaw 安装，将进行全新配置。",
      DetectPath: "路径",
      MigrateBtn: "迁移数据",
      MigrateSkip: "跳过，全新安装",
      Migrating: "迁移中...",
      MigrateDone: "迁移完成",
      MigrateFailed: "迁移失败",

      Step1Title: "选择语言",
      Step1Sub: "选择 rsclaw 系统提示词和回复的主要语言。",

      Step2Title: "选择 LLM 提供商",
      Step2Sub: "选择模型提供商并填入 API Key。可以同时配置多个，rsclaw 会自动故障转移。",

      Step3Title: "连接消息通道",
      Step3Sub: "选择你希望 rsclaw 接入的通道。至少选一个，后续可在配置管理中增删。",
      WechatScanQR: "扫码登录微信",
      WechatScanning: "等待扫码...",
      WechatConnected: "微信已连接",
      WechatExpired: "二维码已过期，请重试",
      FeishuAppId: "飞书 App ID",
      FeishuAppSecret: "飞书 App Secret",

      Step4Title: "启动并验证",
      Step4Sub: "一键启动 rsclaw 网关，自动运行健康检查，确保一切就绪。",
      LaunchGateway: "启动网关",
      Launching: "启动中...",
      StartChatting: "开始聊天",
      CheckGateway: "网关进程启动",
      CheckHealth: "GET /health 检查",
      CheckChannel: "通道连接验证",
      CheckModel: "模型接口测试",
      StatusPass: "通过",
      StatusChecking: "检查中...",
      StatusWaiting: "等待中",
      ReadyTitle: "rsclaw 已就绪!",
      ReadySub: "聊天端点已自动切换到 localhost:18888，你可以立即开始使用。",
      Summary: (lang: string, providers: string, channels: string) =>
        `语言: ${lang} | 提供商: ${providers} | 通道: ${channels || "无"}`,
    },
  },
};

type DeepPartial<T> = T extends object
  ? {
      [P in keyof T]?: DeepPartial<T[P]>;
    }
  : T;

export type LocaleType = typeof cn;
export type PartialLocaleType = DeepPartial<typeof cn>;

export default cn;
