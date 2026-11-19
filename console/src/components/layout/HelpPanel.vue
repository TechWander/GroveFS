<script setup lang="ts">
/*
  "What does this button do", in the page.

  Collapsed by default: an operator who knows the verbs never pays for the
  explanation in scroll distance, and one who does not can search it — it is text
  in the DOM, not a title attribute nobody hovers.
*/
import { t } from '@/i18n'

const props = defineProps<{
  summary: string
  pairs: [string, string][]
}>()
</script>

<template>
  <details class="help">
    <summary>{{ t(props.summary) }}</summary>
    <div class="body">
      <dl class="gloss">
        <template v-for="[k, v] in props.pairs" :key="k">
          <dt>{{ t(k) }}</dt>
          <dd>{{ t(v) }}</dd>
        </template>
      </dl>
    </div>
  </details>
</template>
