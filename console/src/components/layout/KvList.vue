<!--
  The label/value grid a record is read in.

  ONE `<dl>` per section, so the label column stays aligned down the whole block
  — which is why this is a list component with row children rather than a row
  component that brings its own wrapper.

  It takes no props on purpose: the gateway's CSP has no `unsafe-inline`, so a
  style bound from a component (a width, a colour) would be silently dropped by
  the browser. Everything in this project is a class the stylesheet already
  knows.
-->
<template>
  <dl class="kv">
    <slot />
  </dl>
</template>
