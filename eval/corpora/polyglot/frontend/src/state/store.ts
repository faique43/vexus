/** A minimal, framework-free observable state container. */

type Listener<T> = (state: T) => void;

/** A single observable slice of application state. */
export class Store<T> {
    private state: T;
    private listeners: Listener<T>[] = [];

    constructor(initial: T) {
        this.state = initial;
    }

    /** Return the current state snapshot. */
    getState(): T {
        return this.state;
    }

    /** Replace the state and notify every subscriber. */
    setState(next: T): void {
        this.state = next;
        for (const listener of this.listeners) {
            listener(next);
        }
    }

    /** Register a listener invoked on every future `setState`. */
    subscribe(listener: Listener<T>): void {
        this.listeners.push(listener);
    }
}
