import { windowControl } from "../lib/backend";

export function TitleBar() {
  return (
    <div className="relative z-50 flex h-8 shrink-0 items-stretch border-b border-line bg-rail" data-tauri-drag-region>
      <div className="min-w-0 flex-1" data-tauri-drag-region />
      <button className="w-11 text-[#d6d6d6] hover:bg-white/10" onClick={() => void windowControl("minimize")} aria-label="Minimize">
        <span className="block text-lg leading-none">&#8211;</span>
      </button>
      <button className="w-11 text-[#d6d6d6] hover:bg-white/10" onClick={() => void windowControl("toggle")} aria-label="Maximize">
        <span className="block text-[11px] leading-none">&#9633;</span>
      </button>
      <button className="w-11 text-mute hover:bg-[#c84040] hover:text-white" onClick={() => void windowControl("close")} aria-label="Close">
        <span className="block text-lg leading-none">&#215;</span>
      </button>
    </div>
  );
}
