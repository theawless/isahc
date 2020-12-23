/// Helper macro that allows you to attempt to downcast a generic type, as long
/// as it is known to be `'static`.
macro_rules! match_type {
    {
        $(
            <$name:ident as $T:ty> => $branch:expr,
        )*
        $defaultName:ident => $defaultBranch:expr,
    } => {{
        match () {
            $(
                _ if ::std::any::Any::type_id(&$name) == ::std::any::TypeId::of::<$T>() => {
                    #[allow(unsafe_code)]
                    let $name: $T = unsafe {
                        ::std::mem::transmute_copy::<_, $T>(&::std::mem::ManuallyDrop::new($name))
                    };
                    $branch
                }
            )*
            _ => $defaultBranch,
        }
    }};
}

macro_rules! decl_future {
    (
        $(
            $(#[$meta:meta])*
            $vis:vis type $ident:ident $(< $($T:ident),* >)? = async $output:ty $(where Send if $S:ident)?;
        )*
    ) => {
        $(
            decl_future! {
                @impl
                $(#[$meta])*
                $vis type $ident$(<$($T),*>)* = async $output $(where Send if $S)*;
            }
        )*
    };

    (
        @impl
        $(#[$meta:meta])*
        $vis:vis type $ident:ident $(< $($T:ident),+ >)? = async $output:ty where Send if $S:ident;
    ) => {
        $(#[$meta])*
        #[allow(missing_debug_implementations)]
        #[must_use = "futures do nothing unless you `.await` or poll them"]
        $vis struct $ident<'a $( $(, $T)* )*> {
            inner: $crate::util::MaybeSend<$S, ::std::pin::Pin<Box<dyn ::std::future::Future<Output = $output> + 'a>>>,
        }

        $(#[$meta])*
        impl<'a $( $(, $T)* )*> $ident<'a $( $(, $T)* )*> {
            #[inline]
            #[allow(unsafe_code, unused)]
            pub(crate) fn new<F>(f: F) -> Self
            where
                F: ::std::future::Future<Output = $output> + 'a,
            {
                Self {
                    inner: unsafe {
                        $crate::util::MaybeSend::new_unchecked(Box::pin(f))
                    },
                }
            }
        }

        $(#[$meta])*
        impl<$( $($T)* )*> ::std::future::Future for $ident<'_ $( $(, $T)* )*>
        where
            $S: Unpin,
        {
            type Output = $output;

            fn poll(self: ::std::pin::Pin<&mut Self>, cx: &mut ::std::task::Context<'_>) -> ::std::task::Poll<Self::Output> {
                self.get_mut().inner.as_mut().as_mut().poll(cx)
            }
        }
    };

    (
        @impl
        $(#[$meta:meta])*
        $vis:vis type $ident:ident $(< $($T:ident),+ >)? = async $output:ty;
    ) => {
        $(#[$meta])*
        #[allow(missing_debug_implementations)]
        #[must_use = "futures do nothing unless you `.await` or poll them"]
        $vis struct $ident<'a $( $(, $T)* )*> {
            inner: ::std::pin::Pin<Box<dyn ::std::future::Future<Output = $output> + 'a>>,
        }

        $(#[$meta])*
        impl<'a $( $(, $T)* )*> $ident<'a $( $(, $T)* )*> {
            #[inline]
            #[allow(unused)]
            pub(crate) fn new<F>(f: F) -> Self
            where
                F: ::std::future::Future<Output = $output> + 'a,
            {
                Self {
                    inner: Box::pin(f),
                }
            }
        }

        $(#[$meta])*
        impl<$( $($T)* )*> ::std::future::Future for $ident<'_ $( $(, $T)* )*> {
            type Output = $output;

            fn poll(mut self: ::std::pin::Pin<&mut Self>, cx: &mut ::std::task::Context<'_>) -> ::std::task::Poll<Self::Output> {
                self.inner.as_mut().poll(cx)
            }
        }
    };
}
