<script context="module" lang="ts">
  export const currency = 'EUR';
</script>

<script lang="ts">
  import { applyTax } from './billing';

  export let amount: number;

  function taxed(value: number): string {
    return `${applyTax(value)} ${currency}`;
  }
</script>

<tr>
  <td>{taxed(amount)}</td>
</tr>

<style>
  td { text-align: right; }
</style>
