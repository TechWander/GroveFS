import { computed, ref } from 'vue'
import { defineStore } from 'pinia'
import { useOps } from '@/composables/useOps'
import { messageOf } from '@/utils/error'
import { mergeViews } from '@/utils/views'
import type { ViewRow } from '@/types'

/*
  The namespace list, shared by three pages (key bindings, delegations, the
  view picker).

  It is cached, and refreshed on every page load, because a base view created in
  another tab is a thing people expect to see immediately.
*/

export const useCatalogStore = defineStore('catalog', () => {
  const { api } = useOps()

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
  }

  return { views, pending, error, sel, baseNames, byName, loadViews, reset }
})
