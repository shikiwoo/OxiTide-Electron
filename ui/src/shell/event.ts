// Shim for @tauri-apps/api/event in the Electron shell.
import { addListener } from "./bridge";

export type UnlistenFn = () => void;
export interface Event<T> {
  event: string;
  id: number;
  payload: T;
}
export type EventCallback<T> = (event: Event<T>) => void;

export async function listen<T>(event: string, handler: EventCallback<T>): Promise<UnlistenFn> {
  return addListener(event, handler as never);
}

export async function once<T>(event: string, handler: EventCallback<T>): Promise<UnlistenFn> {
  const un = addListener(event, (e) => {
    un();
    (handler as never as (e: unknown) => void)(e);
  });
  return un;
}
