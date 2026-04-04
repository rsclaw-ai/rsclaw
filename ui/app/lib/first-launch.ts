import { safeLocalStorage } from "@/app/utils";

const SETUP_KEY = "rsclaw_setup_complete";
const localStorage = safeLocalStorage();

export function isFirstLaunch(): boolean {
  return !localStorage.getItem(SETUP_KEY);
}

export function markSetupComplete() {
  localStorage.setItem(SETUP_KEY, "true");
}

export function resetSetup() {
  localStorage.removeItem(SETUP_KEY);
}
