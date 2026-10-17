"use client"

import { DeviceControls } from "@/features/identity/device-controls"
import { useWorkspace } from "./context"

export function DevicesScreen() {
  const workspace = useWorkspace()
  return (
    <section className="workspace-screen" aria-label="我的设备" hidden={workspace.view !== "devices"}>
      <DeviceControls />
    </section>
  )
}
