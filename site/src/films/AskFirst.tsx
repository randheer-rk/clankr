import { AbsoluteFill, interpolate, spring, useCurrentFrame, useVideoConfig } from "remotion"
import { clamp, Lower, Stage, Sweep, t, Words } from "./kit"

export const ASK_FRAMES = 360

export function AskFirst() {
  const f = useCurrentFrame()
  const { fps } = useVideoConfig()
  const fadeIn = t(f, 0, 24), fadeOut = 1 - t(f, 336, 360)
  // the card rises out of the dark, tilted, and settles
  const rise = spring({ frame: f - 30, fps, config: { damping: 18, stiffness: 70, mass: 1.1 } })
  // the press: a ring fills while the button is held
  const hold = interpolate(f, [128, 176], [0, 1], clamp)
  const pressed = f >= 128 && f < 180
  const done = f >= 176
  const stamp = spring({ frame: f - 186, fps, config: { damping: 11, stiffness: 160 } })
  const leave = t(f, 268, 316)
  const entry = t(f, 284, 312)

  return (
    <Stage glow="#2b2238" x={50} y={46}>
      <AbsoluteFill style={{ opacity: fadeIn * fadeOut }}>
        <AbsoluteFill style={{ perspective: 1600 }}>
          <div style={{
            position: "absolute", left: "50%", top: 150, width: 760, marginLeft: -380,
            transform: `translateY(${(1 - rise) * 420 - leave * 90}px) rotateX(${(1 - rise) * 38 + leave * 8}deg) rotateY(${(1 - rise) * -12}deg) scale(${1 - leave * 0.18})`,
            opacity: rise * (1 - leave * 0.55),
          }}>
            <div style={{ position: "relative", borderRadius: 40, padding: "40px 44px 38px", overflow: "hidden", border: "1.5px solid rgba(255,255,255,.2)",
              background: "linear-gradient(160deg, rgba(255,255,255,.12), rgba(255,255,255,.03) 55%, rgba(190,160,255,.05))", boxShadow: "0 80px 140px -50px rgba(0,0,0,.9), inset 0 1px 0 rgba(255,255,255,.28)" }}>
              <Sweep at={70} dur={60} strength={0.16} />
              <div style={{ display: "flex", alignItems: "center", gap: 16, fontSize: 22, color: "rgba(245,243,239,.6)" }}>
                <span style={{ width: 44, height: 44, borderRadius: 14, display: "grid", placeItems: "center", background: "rgba(255,122,61,.16)", color: "#ffb37a", fontWeight: 700 }}>W</span>
                Your writer would like to
                <span style={{ marginLeft: "auto", fontFamily: "'JetBrains Mono', monospace", fontSize: 18, opacity: 1 - Math.min(1, stamp * 2) }}>09:44</span>
              </div>
              <div style={{ fontSize: 46, fontWeight: 600, letterSpacing: "-0.03em", lineHeight: 1.1, marginTop: 22 }}>Send the spring newsletter<br />to 2,340 people</div>
              <div style={{ marginTop: 18, fontSize: 22, color: "rgba(245,243,239,.55)", lineHeight: 1.45 }}>“Spring is here, and so is 20% off everything blue…” · reviewed by your editor</div>
              <div style={{ display: "flex", gap: 14, marginTop: 34 }}>
                <div style={{ position: "relative", flex: 1, height: 72, borderRadius: 22, overflow: "hidden", display: "flex", alignItems: "center", justifyContent: "center", gap: 14, fontSize: 25, fontWeight: 600,
                  background: done ? "#2f8f5b" : "#f5f3ef", color: done ? "#f5f3ef" : "#111", transform: `scale(${pressed ? 0.97 : 1})`, transition: "none" }}>
                  {!done && <div style={{ position: "absolute", inset: 0, width: `${hold * 100}%`, background: "linear-gradient(90deg, #ffd3b5, #ffb37a)" }} />}
                  <span style={{ position: "relative" }}>{done ? "✓  Sent" : pressed ? "Keep holding…" : "Hold to allow"}</span>
                </div>
                <div style={{ width: 170, height: 72, borderRadius: 22, display: "grid", placeItems: "center", fontSize: 25, fontWeight: 600, border: "1.5px solid rgba(255,255,255,.22)", color: "rgba(245,243,239,.75)", opacity: done ? 0.3 : 1 }}>Not now</div>
              </div>
              {/* the stamp: who allowed it, and when */}
              <div style={{ position: "absolute", right: 30, top: 30, padding: "8px 16px", borderRadius: 14, border: "3px solid #8fe0ad", color: "#8fe0ad", fontFamily: "'JetBrains Mono', monospace", fontSize: 18, fontWeight: 600, letterSpacing: ".08em",
                opacity: Math.min(1, stamp * 1.4), transform: `rotate(-4deg) scale(${2.2 - stamp * 1.2})` }}>ALLOWED BY YOU · 09:46</div>
            </div>
          </div>
          {/* the press, seen as a soft ring of light where a finger would be */}
          {f >= 120 && f < 186 && (
            <div style={{ position: "absolute", left: 662, top: 465, width: 90, height: 90, borderRadius: "50%", border: "2px solid rgba(255,211,181,.8)", boxShadow: "0 0 40px rgba(255,179,122,.5)",
              opacity: interpolate(f, [120, 130, 176, 186], [0, 1, 1, 0], clamp), transform: `scale(${0.8 + hold * 0.25})` }} />
          )}
        </AbsoluteFill>

        {/* the answer goes into the logbook */}
        <div style={{ position: "absolute", left: "50%", top: 546, width: 760, marginLeft: -380, display: "flex", gap: 26, alignItems: "center", padding: "20px 30px", borderRadius: 22,
          background: "rgba(255,255,255,.05)", border: "1px solid rgba(255,255,255,.12)", opacity: entry * (1 - t(f, 326, 340)), transform: `translateY(${(1 - entry) * 24}px)` }}>
          <span style={{ fontFamily: "'JetBrains Mono', monospace", fontSize: 21, color: "rgba(245,243,239,.45)" }}>09:46</span>
          <span style={{ width: 11, height: 11, borderRadius: 6, background: "#8fe0ad", boxShadow: "0 0 14px #8fe0ad" }} />
          <span style={{ fontSize: 28 }}>You said yes. Newsletter sent.</span>
        </div>

        <div style={{ position: "absolute", left: 0, right: 0, top: 70 }}>
          <Words text="Some things need a yes." at={8} size={52} out={110} style={{ color: "rgba(245,243,239,.9)" }} />
        </div>
        <div style={{ position: "absolute", left: 0, right: 0, top: 70 }}>
          <Words text="Yours." at={190} size={52} out={262} style={{ color: "rgba(245,243,239,.9)" }} />
        </div>
        <div style={{ position: "absolute", left: 0, right: 0, top: 660 }}>
          <Words text="You decide what goes out." at={300} size={86} />
        </div>
        <Lower text="It asks. It waits. It writes down your answer." at={318} style={{ top: 774 }} />
      </AbsoluteFill>
    </Stage>
  )
}
