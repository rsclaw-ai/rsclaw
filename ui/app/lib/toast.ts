// Global toast notification system for rsclaw UI.
// Usage: import { toast } from '@/app/lib/toast';
//        toast.error("保存失败", "网关未运行");
//        toast.success("已保存");

type ToastType = "error" | "warn" | "success";

interface ToastItem {
  id: string;
  type: ToastType;
  title: string;
  desc?: string;
  ts: number;
}

let _listeners: ((items: ToastItem[]) => void)[] = [];
let _items: ToastItem[] = [];
let _counter = 0;

function notify() {
  _listeners.forEach((fn) => fn([..._items]));
}

function add(type: ToastType, title: string, desc?: string) {
  const id = `toast-${++_counter}`;
  _items.push({ id, type, title, desc, ts: Date.now() });
  notify();
  setTimeout(() => remove(id), 4000);
}

function remove(id: string) {
  _items = _items.filter((t) => t.id !== id);
  notify();
}

// Translate raw error messages to user-friendly text.
function translateError(msg: string): string {
  const lower = msg.toLowerCase();
  if (lower.includes("load failed") || lower.includes("failed to fetch"))
    return "网关连接失败，请检查网关是否正在运行";
  if (lower.includes("network"))
    return "网络请求失败，请稍后重试";
  if (lower.includes("timeout"))
    return "请求超时，网关响应时间过长";
  if (lower.includes("permission"))
    return "权限不足，请检查配置文件权限";
  if (lower.includes("unauthorized"))
    return "认证失败，请检查 Auth Token";
  return "操作失败，请查看实时日志获取详情";
}

export const toast = {
  error(title: string, desc?: string) {
    add("error", title, desc);
  },
  warn(title: string, desc?: string) {
    add("warn", title, desc);
  },
  success(title: string, desc?: string) {
    add("success", title, desc);
  },
  // Convenience: show error from a caught exception with translation.
  fromError(title: string, err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    add("error", title, translateError(msg));
  },
  remove,
  subscribe(fn: (items: ToastItem[]) => void) {
    _listeners.push(fn);
    return () => {
      _listeners = _listeners.filter((l) => l !== fn);
    };
  },
  getItems() {
    return [..._items];
  },
};

export type { ToastItem, ToastType };
