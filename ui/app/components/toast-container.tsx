"use client";

import { useEffect, useState } from "react";
import { toast, ToastItem } from "../lib/toast";

const COLORS: Record<string, { border: string; line: string }> = {
  error: { border: "rgba(217,95,95,0.25)", line: "#d95f5f" },
  warn: { border: "rgba(249,115,22,0.25)", line: "#f97316" },
  success: { border: "rgba(45,212,160,0.25)", line: "#2dd4a0" },
};

export function ToastContainer() {
  const [items, setItems] = useState<ToastItem[]>([]);

  useEffect(() => {
    return toast.subscribe(setItems);
  }, []);

  if (items.length === 0) return null;

  return (
    <div
      style={{
        position: "fixed",
        top: 16,
        right: 16,
        zIndex: 9999,
        display: "flex",
        flexDirection: "column",
        gap: 8,
        pointerEvents: "none",
      }}
    >
      {items.map((item) => {
        const c = COLORS[item.type] || COLORS.error;
        return (
          <div
            key={item.id}
            style={{
              pointerEvents: "auto",
              width: 300,
              background: "#1a1c22",
              border: `1px solid ${c.border}`,
              borderLeft: `3px solid ${c.line}`,
              borderRadius: 10,
              padding: "12px 14px",
              display: "flex",
              gap: 10,
              animation: "fadein 0.2s ease",
              boxShadow: "0 8px 24px rgba(0,0,0,0.5)",
            }}
          >
            <div style={{ flex: 1, minWidth: 0 }}>
              <div
                style={{
                  fontSize: 13,
                  fontWeight: 600,
                  color: "#eceaf4",
                  marginBottom: item.desc ? 4 : 0,
                }}
              >
                {item.title}
              </div>
              {item.desc && (
                <div
                  style={{
                    fontSize: 11,
                    color: "#6a6878",
                    lineHeight: 1.5,
                  }}
                >
                  {item.desc}
                </div>
              )}
            </div>
            <div
              style={{
                cursor: "pointer",
                color: "#3e3c4a",
                fontSize: 14,
                lineHeight: 1,
                flexShrink: 0,
              }}
              onClick={() => toast.remove(item.id)}
            >
              x
            </div>
          </div>
        );
      })}
    </div>
  );
}
