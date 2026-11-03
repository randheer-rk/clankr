import { lazy, Suspense } from "react"

// Remotion is the heaviest dependency: load it, and each scene, only when the page needs it
const Crash = lazy(async () => {
  const [{ Scene }, { CrashResume, CRASH_FRAMES }] = await Promise.all([import("./Scene"), import("../scenes/CrashResume")])
  return { default: () => <Scene component={CrashResume} frames={CRASH_FRAMES} still={300} label="A run survives a node crash: the task moves from node-a to node-b and the event log shows every step" /> }
})
const Talk = lazy(async () => {
  const [{ Scene }, { Messaging, MSG_FRAMES }] = await Promise.all([import("./Scene"), import("../scenes/Messaging")])
  return { default: () => <Scene component={Messaging} frames={MSG_FRAMES} still={140} label="A request to a service by name gets a reply from one instance; a topic fans out into two mailboxes and a trigger" /> }
})

const hold = <div className="scene" style={{ aspectRatio: "940 / 340" }} />
export const CrashScene = () => <Suspense fallback={hold}><Crash /></Suspense>
export const TalkScene = () => <Suspense fallback={hold}><Talk /></Suspense>
