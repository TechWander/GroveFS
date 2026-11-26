import { computed, ref } from 'vue'
import { defineStore } from 'pinia'
import { useOps } from '@/composables/useOps'
import { storageEngine } from '@/utils/engine'
import { messageOf } from '@/utils/error'
import { mergeViews } from '@/utils/views'
import type { StoreInfo, ViewRow } from '@/types'

/*
  The namespace list, shared by three pages (key bindings, delegations, the
  view picker).

  It is cached, and refreshed on every page load, because a base view created in
  another tab is a thing people expect to see immediately.
*/

export const useCatalogStore = defineStore('catalog', () => {
  const { api, soft } = useOps()

  const views = ref<ViewRow[]>([])
  const pending = ref(false)
  const error = ref('')
  /** The selected view name. It outlives a page switch on purpose: the
      accounts page's "open this base view" pill and the overview's delegation
      list both jump HERE, and a picker that forgets what you clicked is a
      picker you use twice. */
  const sel = ref<string | null>(null)

  const baseNames = computed(() => views.value.filter((v) => v.kind === 'base').map((v) => v.name))
  const byName = computed(() => new Map(views.value.map((v) => [v.name, v])))

  /* The deployment's storage picture, read once per session. `store.info` is
     Super-only, so a delegated account gets `null` and the list simply omits
     the instance column — never a fabricated "local". */
  const storage = ref<StoreInfo['storage'] | null>(null)
  const storagePending = ref(false)

  /** Which engine carries the trees, read from the storage snapshot alone —
      never from the control plane's name, which says `sqlite` on a treedb
      deployment and would switch this store to the wrong layout. */
  const engine = computed(() => storageEngine(storage.value))

  async function loadStorage(force = false): Promise<void> {
    if (storage.value && !force) return
    if (storagePending.value) return
    storagePending.value = true
    try {
      const res = await soft('store.info', {})
      if (res.ok) storage.value = res.r?.storage ?? null
    } finally {
      storagePending.value = false
    }
  }

  /** Which instance hosts a view. The answer means something different per
      engine, so the engine decides which book is read:
        · treedb — no placement ledger exists; the snapshot carries each
          server's live `viewNames`, and a view on an unreachable server is
          simply unanswerable (`null`), NOT the PG word for "routed by hash".
        · PostgreSQL — the placement row wins; a single logical instance
          answers for every name; anything else is hash-routed.
        · SQLite — one embedded store, so there is no instance question to ask
          (callers hide the column; see `showsInstance`). */
  function instanceOf(view: string): string | null {
    const s = storage.value
    if (!s) return null
    if (s.backend === 'treedb') {
      return s.instances.find((i) => i.viewNames?.includes(view))?.name ?? null
    }
    if (s.placement[view]) return s.placement[view]
    return s.instances.length === 1 ? s.instances[0].name : null
  }

  /** Whether "which instance?" is a question this deployment can answer. On
      SQLite every tree sits in the same local store, so printing `local` on
      every row would be noise, not information. */
  const showsInstance = computed(
    () => engine.value === 'postgres' || engine.value === 'treedb',
  )

  /** `force` re-reads even a populated list; a page that needs the namespace at
      its freshest (the views page, right after a create) asks for it. */
  async function loadViews(force = false): Promise<ViewRow[]> {
    if (views.value.length && !force) return views.value
    pending.value = true
    error.value = ''
    try {
      const [phys, defs] = await Promise.all([api('physical.list', {}), api('def.list', {})])
      views.value = mergeViews(phys ?? [], defs ?? [])
    } catch (e) {
      error.value = messageOf(e)
    } finally {
      pending.value = false
    }
    return views.value
  }

  function reset() {
    views.value = []
    sel.value = null
    error.value = ''
    storage.value = null
  }

  return {
    views,
    pending,
    error,
    sel,
    baseNames,
    byName,
    storage,
    storagePending,
    engine,
    showsInstance,
    loadViews,
    loadStorage,
    instanceOf,
    reset,
  }
})
