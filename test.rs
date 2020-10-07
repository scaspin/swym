extern crate swym;
fn main() {
    use swym::thread_key;
    let thread_key = thread_key::get();
    use swym::tcell::TCell;

    static A: TCell<i32> = TCell::new(0);
    let b = TCell::new(42);

    thread_key.rw(|tx| {
        let temp = A.get(tx, Default::default())?;
        A.set(tx, b.get(tx, Default::default())?)?;
        b.set(tx, temp)?;
        Ok(())
    });
}
