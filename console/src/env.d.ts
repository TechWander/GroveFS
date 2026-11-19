/*
  A fallback type for `*.vue` imports, for the one tool in this project that is
  NOT vue-tsc: ESLint's TypeScript program.

  vue-tsc loads Vue's language plugin and resolves `@/pages/KeysPage.vue` to the
  real component type, so `npm run typecheck` verifies props, emits and template
  expressions against the actual SFC. The plain `typescript` API ESLint drives
  cannot read a `.vue` file, so without this declaration every component import
  it touches comes back error-typed and the type-aware rules fire on code that
  is fine.

  This shim does not weaken the real check: an ambient wildcard is consulted
  only when resolution fails, and under vue-tsc it never does — a wrong prop on
  a `.vue` component still reports from `npm run typecheck`, as verified.
*/

declare module '*.vue' {
  import type { DefineComponent } from 'vue'
  const component: DefineComponent<Record<string, unknown>, Record<string, unknown>, unknown>
  export default component
}
