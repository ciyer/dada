# Leasing permissions

import Caveat from '../caveat.md'

<Caveat/>

So far in the tutorial, we've worked exclusively with the two **owned** permissions, `my` and `our`. Owned permissions have the property that they are independent from all other variables. When you have owned permission to an object, [nobody can take that away from you](./our.md#why-cant-we-invalidate-other-our-values). (In the case of `our`, you might only have read permission, but that read permission cannot be revoked.)

Sometimes, though, we want to give people **temporary** access to an object that we own, without giving up control. In Dada, this is called a **lease**, and it works in a very similar way to how an ["at will" lease] works in "real life". If one variable `a` owns an object, it can lease out that object to another variable `b` temporarily. `b` can use the object up until `a` wants it back.

["at will" lease]: https://en.wikipedia.org/wiki/Leasehold_estate#Tenancy_at_will

## The permissions table

Mutable permissions complete the "permissions table" that we saw earlier:

|            | Unique             | Shared                     |
| ---------- | ------------------ | -------------------------- |
| Owned      | [`my`](./my.md)    | [`our`](./our.md)          |
| **Mutable** | ⭐ **`mutable`** ⭐ | [`shleased`](./shlease.md) |

We'll start by talking about _unique leases_, written `lease`, and then cover _shared leases_, which we call a `shlease` (pronounced, um, "sh-lease", kind of like the German "schliese"[^notreal]).

[^notreal]: No, "shlease" is not a real word.^[wasnot]
[^wasnot]: At least, "shlease" _was_ not a real word, until now! Who wants words that other people have invented anyway?

## Unique leases

In this example, the line `let l: mutable = p` declares a variable `l` that has a _unique lease_ to the object stored in `p`:

```
class Point(x: our, y: our)

let p: my = Point(22, 44)
let l: mutable = p
l.x += 1
print(l.x).await             # Prints `23`
print(p.x).await             # Prints `23`
```

As you can see, a unique lease can be used just like the original object. Modifications to the mutable object like `l.x += 1` affect the original object too, so when we print `p.x` we see `23`.

If you position your cursor right after `let l: mutable = p`, you will see the following diagram, which shows that both `p` and `l` store the same object. The `my` arrow from `p` has been "dashed" to indicate that, while `p` retains ownership, the object is currently mutable to another variable:

```
┌───┐
│   │                  ┌───────┐
│ p ├╌my╌╌╌╌╌╌╌╌╌╌╌╌╌╌►│ Point │
│   │                  │ ───── │
│ l ├─mutable──────────►│ x: 22 │
│   │                  │ y: 44 │
└───┘                  └───────┘
```

## What makes unique leases _unique_?

Unique leases are called unique because, so long as the lease has not been canceled, it grants unique access to the object, meaning that no other variable besides `l` can access the object in any way -- even reads. This is why its ok to write to the object through `l`, even though ["friends don't let friends mutate shared data"](./sharing_xor_mutation.md). In other words, even though two variables have access to the same object (`p` and `l`), only one of them has access to the object _at any given time_.

## Terminating a unique lease

A unique lease can be _terminated_ in two ways:

-   The mutable variable `l` goes out of scope.
-   The owner (or lessor) variable `p` is used again. Because `l` had a unique lease, once `p` starts using the object, that implies the unique lease is canceled -- otherwise it wouldn't be unique, would it?

You can see this second case by positioning your cursor after the `print(p).await` line in our original example. You will see that the `l` variable no longer has a value, and the `p` line is no longer 'dashed':

```
class Point(x: our, y: our)

let p: my = Point(22, 44)
let l: mutable = p
l.x += 1
print(l.x).await             # Prints `23`
print(p.x).await             # Prints `23`
#               ▲
# ──────────────┘

# You see
#
# ┌───┐
# │   │                  ┌───────┐
# │ p ├─my──────────────►│ Point │
# │   │                  │ ───── │
# │ l │                  │ x: 23 │
# │   │                  │ y: 44 │
# └───┘                  └───────┘
```

What do you think will happen if we try to use `l` again? Try it and find out!

## The `lease` keyword

In addition to assigning to a `mutable` variable, we can explicitly create a lease with the `lease` keyword. This is particularly useful when combined with [`any` permissions](./any.md). In this example, `let l: any = p.lease` is equivalent to the `let l: mutable = p` that we saw earlier:

```
class Point(x: our, y: our)

let p: my = Point(22, 44)
let l: any = p.lease
l.x += 1
```

# Giving a mutable value

You may be wondering what happens when you `give` a mutable value. As always, giving a value means creating a second value with the same permissions -- but to explain exactly how giving works on a mutable value, we have to introduce one more concept, the _sublease_. That's covered in the next section.
