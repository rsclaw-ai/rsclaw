import DeleteIcon from "../icons/delete.svg";

import styles from "./home.module.scss";
import {
  DragDropContext,
  Droppable,
  Draggable,
  OnDragEndResponder,
} from "@hello-pangea/dnd";

import { useChatStore } from "../store";

import Locale, { getLang } from "../locales";
import { useLocation, useNavigate } from "react-router-dom";
import { Path } from "../constant";
import { Mask } from "../store/mask";
import { Avatar } from "./emoji";
import { useRef, useEffect, useState, useCallback } from "react";
import { showConfirm } from "./ui-lib";
import { useMobileScreen } from "../utils";
import clsx from "clsx";

export function ChatItem(props: {
  onClick?: () => void;
  onDelete?: () => void;
  onRename?: (newTitle: string) => void;
  title: string;
  count: number;
  time: string;
  selected: boolean;
  id: string;
  index: number;
  narrow?: boolean;
  mask: Mask;
  agentId?: string;
}) {
  const draggableRef = useRef<HTMLDivElement | null>(null);
  const [editing, setEditing] = useState(false);
  const [editValue, setEditValue] = useState(props.title);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (props.selected && draggableRef.current) {
      draggableRef.current?.scrollIntoView({
        block: "center",
      });
    }
  }, [props.selected]);

  useEffect(() => {
    if (editing && inputRef.current) {
      inputRef.current.focus();
      inputRef.current.select();
    }
  }, [editing]);

  const commitRename = () => {
    const trimmed = editValue.trim();
    if (trimmed && trimmed !== props.title) {
      props.onRename?.(trimmed);
    }
    setEditing(false);
  };

  const handleContextMenu = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
    setEditValue(props.title);
    setEditing(true);
  }, [props.title]);

  const handleDoubleClick = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
    setEditValue(props.title);
    setEditing(true);
  }, [props.title]);

  const { pathname: currentPath } = useLocation();
  return (
    <Draggable draggableId={`${props.id}`} index={props.index}>
      {(provided) => (
        <div
          className={clsx(styles["chat-item"], {
            [styles["chat-item-selected"]]:
              props.selected &&
              (currentPath === Path.Chat || currentPath === Path.Home),
          })}
          onClick={props.onClick}
          onContextMenu={handleContextMenu}
          ref={(ele) => {
            draggableRef.current = ele;
            provided.innerRef(ele);
          }}
          {...provided.draggableProps}
          {...provided.dragHandleProps}
          title={`${props.title}\n${Locale.ChatItem.ChatItemCount(
            props.count,
          )}`}
        >
          {props.narrow ? (
            <div className={styles["chat-item-narrow"]}>
              <div className={clsx(styles["chat-item-avatar"], "no-dark")}>
                <Avatar
                  avatar={props.mask.avatar}
                />
              </div>
              <div className={styles["chat-item-narrow-count"]}>
                {props.count}
              </div>
            </div>
          ) : (
            <>
              {editing ? (
                <input
                  ref={inputRef}
                  className={styles["chat-item-rename-input"]}
                  value={editValue}
                  onChange={(e) => setEditValue(e.target.value)}
                  onBlur={commitRename}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") commitRename();
                    if (e.key === "Escape") setEditing(false);
                  }}
                  onClick={(e) => e.stopPropagation()}
                />
              ) : (
                <div
                  className={styles["chat-item-title"]}
                  onDoubleClick={handleDoubleClick}
                >
                  {props.title}
                </div>
              )}
              <div className={styles["chat-item-info"]}>
                <div className={styles["chat-item-count"]}>
                  {props.agentId && (
                    <span className={styles["chat-item-agent"]}>{props.agentId}</span>
                  )}
                  {Locale.ChatItem.ChatItemCount(props.count)}
                </div>
                <div className={styles["chat-item-date"]}>{props.time}</div>
              </div>
            </>
          )}

          <div
            className={styles["chat-item-delete"]}
            onClickCapture={(e) => {
              props.onDelete?.();
              e.preventDefault();
              e.stopPropagation();
            }}
          >
            <DeleteIcon />
          </div>
        </div>
      )}
    </Draggable>
  );
}

export function ChatList(props: { narrow?: boolean }) {
  const [sessions, selectedIndex, selectSession, moveSession] = useChatStore(
    (state) => [
      state.sessions,
      state.currentSessionIndex,
      state.selectSession,
      state.moveSession,
    ],
  );
  const chatStore = useChatStore();
  const navigate = useNavigate();
  const isMobileScreen = useMobileScreen();

  const onDragEnd: OnDragEndResponder = (result) => {
    const { destination, source } = result;
    if (!destination) {
      return;
    }

    if (
      destination.droppableId === source.droppableId &&
      destination.index === source.index
    ) {
      return;
    }

    moveSession(source.index, destination.index);
  };

  return (
    <DragDropContext onDragEnd={onDragEnd}>
      <Droppable droppableId="chat-list">
        {(provided) => (
          <div
            className={styles["chat-list"]}
            ref={provided.innerRef}
            {...provided.droppableProps}
          >
            {sessions.map((item, i) => (
              <ChatItem
                title={item.topic}
                time={new Date(item.lastUpdate).toLocaleString(getLang() === "cn" ? "zh-CN" : undefined)}
                count={item.messages.length}
                key={item.id}
                id={item.id}
                index={i}
                selected={i === selectedIndex}
                onClick={() => {
                  navigate(Path.Chat);
                  selectSession(i);
                }}
                onDelete={async () => {
                  if (
                    (!props.narrow && !isMobileScreen) ||
                    (await showConfirm(Locale.Home.DeleteChat))
                  ) {
                    chatStore.deleteSession(i);
                  }
                }}
                narrow={props.narrow}
                mask={item.mask}
                agentId={item.agentId}
                onRename={(newTitle) => chatStore.renameSession(i, newTitle)}
              />
            ))}
            {provided.placeholder}
          </div>
        )}
      </Droppable>
    </DragDropContext>
  );
}
