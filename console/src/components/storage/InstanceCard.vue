<script setup lang="ts">
/*
  One storage instance card, shared by the PostgreSQL book and the treedb
  inventory.

  The two backends differ in what a card MEANS, and the card says so rather than
  flattening them:

    · PostgreSQL — the first instance carries `grovefs_control`, so it wears the
      `control plane` badge; its URI is not editable in place (that is a
      handover), a non-empty instance cannot be deleted until its views are
      relocated off it.
    · treedb — EXPERIMENTAL, and NO instance carries the control plane (that is
      local SQLite), so a card never claims `control`; the one whose address
      equals the configured `--meta-store` is `serving` (this server's trees).
      Relocation is not wired, so the card offers no Relocate and the page notes
      the inventory-only routing.

  Every write verb is gated on BOTH `managed` (a server-adopted registry) and
  the routed-op list (`session.can`), so an operator-owned registry or a
  non-admin session sees a read-only card instead of a button that only ever
  403s. The card owns no dialog: it emits the intent, the page runs the guarded
  flow (preview → confirm) so the server's own refusal sentence is what the
  operator reads.

  The load meter is class-only (`.seg.on`), never an inline `style` width — the
  gateway's CSP is `style-src 'self'` and would drop a bound width.
*/
import { computed } from 'vue'
import type { StoreInstance } from '@/types'
import { useSessionStore } from '@/stores/session'
import { useNav } from '@/composables/useNav'
import { t } from '@/i18n'
import StateBadge from '@/components/ui/StateBadge.vue'
import CopyButton from '@/components/ui/CopyButton.vue'

const props = defineProps<{
  inst: StoreInstance
  backend: 'postgres' | 'treedb'
  managed: boolean
  /** Views resolved onto this card (PG placement / treedb live list). */
  views: string[]
  /** Busiest card's count, so the meters compare across the grid. */
  maxCount: number
  canHide: boolean
  hidden: boolean
}>()

const emit = defineEmits<{
  addbase: [name: string]
  edit: [inst: StoreInstance]
  setcontrol: [inst: StoreInstance]
  relocate: [inst: StoreInstance]
  remove: [inst: StoreInstance]
  /** The instance, not its name: a name is only an identity while the registry
      stays unique, and the page keys its cards by name+address. */
  toggle: [inst: StoreInstance]
}>()

const session = useSessionStore()
const { openInstance } = useNav()

const SEGS = 8
const count = computed(() => props.views.length)
const filled = computed(() => {
  if (!props.maxCount || count.value === 0) return 0
  return Math.max(1, Math.round((count.value / props.maxCount) * SEGS))
})
const isPg = computed(() => props.backend === 'postgres')
const editable = computed(() => props.managed)
// The control plane's URI (PG) and the serving address (treedb) are not editable
// in place — each is the transaction of a different verb (a handover / a restart).
const uriLocked = computed(() => (isPg.value ? props.inst.control : props.inst.serving === true))
const canCreate = computed(() => session.can('view.create'))
</script>

<template>
  <article
    class="inst-card"
    :class="[
      inst.control ? 'control' : '',
      inst.serving ? 'serving' : '',
      props.backend === 'treedb' ? 'is-treedb' : '',
    ]"
  >
    <header class="inst-head">
      <h3 class="mono">{{ inst.name }}</h3>
      <div class="inst-badges">
        <StateBadge
          v-if="inst.control"
          tone="admin"
          :label="t('control plane')"
          :title="t('keys, definitions, accounts, audit and placement live here')"
        />
        <StateBadge
          v-else-if="inst.serving"
          tone="good"
          :label="t('serving')"
          :title="t('the address this meta-server routes its trees to')"
        />
        <span v-if="props.backend === 'treedb'" class="exp-tag" :title="t('treedb is an EXPERIMENTAL backend')">
          {{ t('experimental') }}
        </span>
      </div>
    </header>

    <div class="inst-uri">
      <div class="inst-uri-label">
        <span>{{ isPg ? t('connection') : t('address') }}</span>
        <span v-if="isPg" class="cred">{{ t('contains credentials') }}</span>
      </div>
      <code v-if="!canHide || !hidden">{{ inst.uri ?? '—' }}</code>
      <code v-else class="masked">••••••••••••••••••••</code>
      <div class="inst-uri-actions">
        <button v-if="canHide" type="button" class="mini" :aria-pressed="hidden" @click="emit('toggle', inst)">
          {{ hidden ? t('Show') : t('Hide') }}
        </button>
        <CopyButton v-if="inst.uri" :text="inst.uri" />
      </div>
    </div>

    <div class="inst-load">
      <div class="inst-meter" :aria-hidden="true">
        <span v-for="i in SEGS" :key="i" :class="['seg', i <= filled ? 'on' : '']"></span>
      </div>
      <span class="inst-count">
        <b>{{ count }}</b>
        {{ t('view(s) here') }}
      </span>
    </div>

    <ul class="inst-views">
      <li v-for="v in views.slice(0, 8)" :key="v" class="mono">{{ v }}</li>
      <li v-if="!views.length" class="muted">{{ t('no view is placed here yet') }}</li>
      <li v-else-if="views.length > 8" class="more" :title="t('open the filtered list for the rest')">
        +{{ views.length - 8 }}
      </li>
    </ul>

    <p v-if="props.backend === 'treedb' && !inst.control" class="inst-note">
      {{ t('control plane: local SQLite') }}
    </p>

    <div class="inst-actions">
      <button type="button" @click="openInstance(inst.name)">{{ t('Open these views') }}</button>
      <button v-if="canCreate" type="button" class="primary" @click="emit('addbase', inst.name)">
        {{ t('New base here') }}
      </button>
    </div>

    <div v-if="editable" class="inst-admin">
      <button type="button" class="mini" :disabled="uriLocked" :title="uriLocked ? (isPg ? t('the control plane URI moves by handover, not edit') : t('the serving address changes with a restart, not here')) : ''" @click="emit('edit', inst)">
        {{ t('Edit URI') }}
      </button>
      <button
        v-if="isPg && !inst.control && session.can('storage.relocate')"
        type="button"
        class="mini"
        :disabled="!views.length"
        :title="views.length ? t('move this instance’s views onto another') : t('nothing to relocate')"
        @click="emit('relocate', inst)"
      >
        {{ t('Relocate') }}
      </button>
      <button
        v-if="isPg && !inst.control && session.can('storage.set_control')"
        type="button"
        class="mini"
        @click="emit('setcontrol', inst)"
      >
        {{ t('Set as control') }}
      </button>
      <button
        v-if="session.can('storage.remove')"
        type="button"
        class="mini danger"
        @click="emit('remove', inst)"
      >
        {{ t('Delete') }}
      </button>
    </div>
  </article>
</template>
