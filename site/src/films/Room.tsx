import { AbsoluteFill, interpolate, useCurrentFrame } from "remotion"
import { clamp, Lower, Orb, Stage, t, Words } from "./kit"

export const ROOM_FRAMES = 360
const S = 300 // cube edge, px

const FACES = [
  { id: "front", tf: `translateZ(${S / 2}px)`, from: "translateZ(900px)" },
  { id: "back", tf: `rotateY(180deg) translateZ(${S / 2}px)`, from: "translateZ(-700px)" },
  { id: "left", tf: `rotateY(-90deg) translateZ(${S / 2}px)`, from: "translateX(-700px)" },
  { id: "right", tf: `rotateY(90deg) translateZ(${S / 2}px)`, from: "translateX(700px)" },
  { id: "top", tf: `rotateX(90deg) translateZ(${S / 2}px)`, from: "translateY(-700px)" },
  { id: "bottom", tf: `rotateX(-90deg) translateZ(${S / 2}px)`, from: "translateY(700px)" },
]

function Face({ tf, from, k, sweep, ripple, scale = 1, dashed = false, alpha = 1 }: { tf: string; from: string; k: number; sweep: number; ripple?: number; scale?: number; dashed?: boolean; alpha?: number }) {
  const size = S * scale
  return (
    <div style={{
      position: "absolute", left: -size / 2, top: -size / 2, width: size, height: size, borderRadius: 26 * scale,
      transform: `${k < 1 ? from.replace(/(-?\d+)px/, (_, n) => `${(+n * (1 - k)).toFixed(1)}px`) : ""} ${tf.replace(`${S / 2}px`, `${size / 2}px`)}`,
      opacity: k * alpha, filter: `blur(${(1 - k) * 10}px)`,
      background: "linear-gradient(140deg, rgba(255,255,255,.13), rgba(255,255,255,.025) 46%, rgba(255,210,180,.05))",
      border: `1.5px ${dashed ? "dashed" : "solid"} rgba(255,255,255,${dashed ? 0.22 : 0.3})`,
      boxShadow: "inset 0 0 70px rgba(255,170,120,.07), inset 0 1px 0 rgba(255,255,255,.25)", overflow: "hidden",
    }}>
      <div style={{ position: "absolute", inset: 0, background: `linear-gradient(120deg, transparent ${sweep - 18}%, rgba(255,255,255,.22) ${sweep}%, transparent ${sweep + 18}%)` }} />
      {ripple !== undefined && ripple > 0 && ripple < 1 && (
        <div style={{ position: "absolute", left: "50%", top: "52%", width: 260 * ripple, height: 260 * ripple, marginLeft: -130 * ripple, marginTop: -130 * ripple, borderRadius: "50%", border: `2px solid rgba(255,150,100,${1 - ripple})`, boxShadow: `0 0 30px rgba(255,122,61,${0.6 * (1 - ripple)})` }} />
      )}
    </div>
  )
}

export function Room() {
  const f = useCurrentFrame()
  const fadeIn = t(f, 0, 30), fadeOut = 1 - t(f, 336, 360)
  // camera: a slow orbit and a gentle push in
  const rotY = interpolate(f, [0, 360], [-34, 22], clamp)
  const rotX = interpolate(f, [0, 360], [-18, -10], clamp)
  const push = interpolate(f, [0, 290], [0.86, 1.04], clamp) * (1 - 0.2 * t(f, 288, 336))
  // the helper tries the right-hand wall and is turned back
  const nudge = f < 150 ? 0 : f < 176 ? interpolate(f, [150, 176], [0, 118], { ...clamp }) : interpolate(f, [176, 206], [118, 0], clamp)
  const ripple = interpolate(f, [174, 214], [0, 1], clamp)
  const sweep = interpolate(f % 150, [0, 150], [-30, 140], clamp)
  const shell = (i: number) => t(f, 224 + i * 22, 270 + i * 22)
  const shiftUp = t(f, 290, 330)

  return (
    <Stage glow="#3d2a1c">
      <AbsoluteFill style={{ opacity: fadeIn * fadeOut }}>
        {/* the light it casts on the floor */}
        <div style={{ position: "absolute", left: "50%", top: 560, width: 760, height: 120, marginLeft: -380, borderRadius: "50%", background: "radial-gradient(closest-side, rgba(255,122,61,.22), transparent)", filter: "blur(12px)", opacity: t(f, 20, 80) * (1 - shiftUp * 0.6), transform: `translateY(${-shiftUp * 60}px)` }} />
        <AbsoluteFill style={{ perspective: 1800, transform: `translateY(${-60 - shiftUp * 60}px)` }}>
          <div style={{ position: "absolute", left: "50%", top: "47%", transformStyle: "preserve-3d", transform: `scale(${push}) rotateX(${rotX}deg) rotateY(${rotY}deg)` }}>
            {FACES.map((fc, i) => (
              <Face key={fc.id} tf={fc.tf} from={fc.from} k={t(f, 46 + i * 9, 96 + i * 9)} sweep={sweep} ripple={fc.id === "right" ? ripple : undefined} />
            ))}
            {[1.25, 1.5].map((sc, i) =>
              FACES.filter(fc => fc.id !== "top").map(fc => (
                <Face key={fc.id + sc} tf={fc.tf} from="translateZ(0px)" k={shell(i)} sweep={sweep} scale={sc} dashed={i === 1} alpha={i ? 0.45 : 0.7} />
              )),
            )}
          </div>
          <Orb size={110} x={nudge} y={-24 + Math.sin(f / 22) * 8} heat={t(f, 10, 60) * (1 + ripple * (1 - ripple) * 1.4)} />
        </AbsoluteFill>

        {/* the walls, named as they arrive */}
        {/* the walls, named as they arrive: one line, under the film's caption */}
        <div style={{ position: "absolute", left: 0, right: 0, top: 772, display: "flex", justifyContent: "center", gap: 46, fontFamily: "'JetBrains Mono', monospace", fontSize: 19, letterSpacing: ".04em" }}>
          {["container", "gVisor", "microVM"].map((label, i) => {
            const k = i === 0 ? t(f, 230, 256) : shell(i - 1)
            return <span key={label} style={{ color: "rgba(245,243,239,.5)", opacity: k * (1 - shiftUp), filter: `blur(${(1 - k) * 6}px)` }}>{label}</span>
          })}
        </div>

        <div style={{ position: "absolute", left: 0, right: 0, top: 676 }}>
          <Words text="It runs code you didn't write." at={128} size={56} out={190} />
        </div>
        <div style={{ position: "absolute", left: 0, right: 0, top: 676 }}>
          <Words text="It reaches nothing you didn't grant." at={206} size={56} out={286} />
        </div>
        <div style={{ position: "absolute", left: 0, right: 0, top: 640 }}>
          <Words text="Sandboxed." at={300} size={110} head />
        </div>
        <Lower text="Rootless container, gVisor, or a Firecracker microVM. Chosen per agent." at={318} style={{ top: 774 }} />
      </AbsoluteFill>
    </Stage>
  )
}
