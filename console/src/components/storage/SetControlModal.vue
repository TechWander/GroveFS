<script setup lang="ts">
/*
  Promote another instance to carry the PostgreSQL control plane.

  This is the ONLY supported way to move `grovefs_control` off the first
  registry instance — and it is a two-phase transaction by design: `set_control`
  STAGES a `@control-pending=` directive and the copy runs at the next start,
  before any listener exists, so no request can write to the old plane while the
  tables move. The dialog must say that plainly: the operator is not flipping a
  switch, they are queueing a restart-gated migration, and the page shows a
  pending banner until the server comes back with the target rotated first.

  The server refuses the cases that would strand the plane (already control, a
  handover already staged, an unreachable current plane, a schema carrying
  foreign keys the built-in copy cannot order) — each is printed verbatim. A
  paraphrase of "restart required" or "move by pg_dump" would hide the one
  action the operator can actually take.
*/
import type { StoreInstance } from '@/types'
import { useOps } from '@/composables/useOps'
import { useUiStore } from '@/stores/ui'
import { messageOf } from '@/utils/error'
import { t } from '@/i18n'
import BaseModal from '@/components/overlay/BaseModal.vue'

const props = defineProps<{ inst: StoreInstance }>()
const emit = defineEmits<{ close: [] }>()

const { api } = useOps()
const ui = useUiStore()

async function run(fail: (msg: string) => void, close: () => void): Promise<void> {
  try {
    const r = await api('storage.set_control', { instance: props.inst.name })
    close()
    ui.toast(r.msg)
    ui.reload()
  } catch (e) {
    fail(messageOf(e))
  }
}
</script>

<template>
  <BaseModal
    :title="'Make ${name} the control plane'"
    :vars="{ name: props.inst.name }"
    :confirm="'Stage handover'"
    @confirm="run"
    @close="emit('close')"
  >
    <p>
      {{
        t(
          'This stages a handover of grovefs_control — keys, definitions, accounts, audit and the placement ledger — to ${name}.',
          { name: props.inst.name },
        )
      }}
    </p>
    <p class="warn">
      {{
        t(
          'Nothing moves yet: the copy runs the next time the meta-server starts, before any request is served. Restart it to finish, and the control badge moves then.',
        )
      }}
    </p>
    <p class="muted">
      {{ t('The current plane must be reachable and hold no foreign-key constraints, or the server refuses and explains why.') }}
    </p>
  </BaseModal>
</template>

<style scoped>
.warn {
  color: var(--warn);
}
</style>
