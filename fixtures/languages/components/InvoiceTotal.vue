<template>
  <section class="invoice">
    <p>Total: {{ total() }}</p>
    <button @click="reset">Reset</button>
  </section>
</template>

<script setup lang="ts">
import { ref } from 'vue'
import { applyTax } from './billing'

const props = defineProps<{ items: number[] }>()
const discount = ref(0)

function total(): number {
  const sum = props.items.reduce((a, b) => a + b, 0)
  return applyTax(sum) - discount.value
}

const reset = () => {
  discount.value = 0
}
</script>

<style scoped>
.invoice { font-weight: bold; }
</style>
