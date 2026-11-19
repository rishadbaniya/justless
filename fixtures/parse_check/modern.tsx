// Valid TSX that an edit may write: generic components and typed props.
import { useState, type ReactNode } from "react";

type Props<T> = {
  items: readonly T[];
  render: (item: T) => ReactNode;
};

export const List = <T,>({ items, render }: Props<T>) => {
  const [open, setOpen] = useState<boolean>(false);
  return (
    <section aria-expanded={open} onClick={() => setOpen((o) => !o)}>
      {items.length > 0 ? items.map((item, i) => <div key={i}>{render(item)}</div>) : <p>Empty</p>}
      <Trans>
        Hello <b>{"world"}</b> &amp; friends
      </Trans>
    </section>
  );
};

export default function App(): JSX.Element {
  const value = { a: 1 } satisfies Record<string, number>;
  return <List<number> items={[value.a]} render={(n) => <span>{n as number}</span>} />;
}
