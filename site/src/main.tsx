import { StrictMode } from "react"
import { createRoot } from "react-dom/client"

// ?film=room&f=120 renders one film, paused on one frame (for checking stills)
const q = new URLSearchParams(location.search)
const root = createRoot(document.getElementById("root")!)
if (q.has("film")) {
  Promise.all([import("@remotion/player"), import("./films"), import("./films/kit")]).then(([{ Player }, { FILMS }, { H, W }]) => {
    const { component, frames } = FILMS[q.get("film") as keyof typeof FILMS]
    const compact = q.get("compact") === "1"
    document.body.style.margin = "0"
    root.render(<Player component={component} durationInFrames={frames} compositionWidth={compact ? 800 : W} compositionHeight={compact ? 1000 : H} fps={30} initialFrame={+(q.get("f") ?? 0)} acknowledgeRemotionLicense style={{ width: "100vw", height: "100vh" }} />)
  })
} else {
  import("./App").then(({ App }) => root.render(<StrictMode><App /></StrictMode>))
}
