struct A(&'static str);
struct B(&'static str);
impl Drop for A { fn drop(&mut self){ println!("drop {}", self.0); }}
impl Drop for B { fn drop(&mut self){ println!("drop {}", self.0); }}

struct Foo { a: A, b: B }

fn main(){ let _f = Foo { a: A("A"), b: B("B") }; }
