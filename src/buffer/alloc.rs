use backend::Facade;
use context::CommandContext;
use context::Context;
use version::Version;
use ContextExt;
use gl;
use libc;
use std::{fmt, mem, ptr, slice, cmp};
use std::rc::Rc;
use std::ops::{Deref, DerefMut};
use GlObject;

use buffer::{BufferType, BufferCreationError};

use version::Api;

/// A buffer in the graphics card's memory.
pub struct Buffer {
    context: Rc<Context>,

    /// OpenGL identifier ; can't be zero.
    id: gl::types::GLuint,

    /// Type of buffer.
    ty: BufferType,

    /// Size in bytes of the buffer.
    size: usize,

    /// A pointer to the persistent mapping of this buffer in memory, if there is one.
    persistent_mapping: Option<*mut libc::c_void>,

    /// If true, then this buffer can only be modified by calls to `glCopyBufferSubData` or through
    /// the persistent mapping.
    immutable: bool,
}

/// A mapping of a buffer.
pub struct Mapping<'b, D> {
    buffer: &'b Buffer,
    temporary_buffer: Option<(gl::types::GLuint, usize)>,
    data: *mut D,
    len: usize,
}

impl Buffer {
    /// Builds a new buffer containing the given data. The size of the buffer is equal to the
    /// size of the data.
    pub fn new<D, F>(facade: &F, data: &[D], ty: BufferType, dynamic: bool)
                     -> Result<Buffer, BufferCreationError>
                     where D: Send + Copy + 'static, F: Facade
    {
        assert!(mem::size_of::<D>() > 0);

        let mut ctxt = facade.get_context().make_current();

        let size = data.len() * mem::size_of::<D>();

        let (id, immutable, persistent_mapping) = try!(unsafe {
            create_buffer(&mut ctxt, size, Some(&data), ty, dynamic, false)
        });

        Ok(Buffer {
            context: facade.get_context().clone(),
            id: id,
            ty: ty,
            size: size,
            persistent_mapping: persistent_mapping,
            immutable: immutable,
        })
    }

    /// Builds a new empty buffer of the given size.
    pub fn empty<F>(facade: &F, ty: BufferType, size: usize, dynamic: bool)
                    -> Result<Buffer, BufferCreationError> where F: Facade
    {
        let mut ctxt = facade.get_context().make_current();

        let (id, immutable, persistent_mapping) = try!(unsafe {
            create_buffer::<()>(&mut ctxt, size, None, ty, dynamic, false)
        });

        Ok(Buffer {
            context: facade.get_context().clone(),
            id: id,
            ty: ty,
            size: size,
            persistent_mapping: persistent_mapping,
            immutable: immutable,
        })
    }

    /// Returns the context corresponding to this buffer.
    pub fn get_context(&self) -> &Rc<Context> {
        &self.context
    }

    /// Returns the total size in bytes of this buffer.
    pub fn get_size(&self) -> usize {
        self.size
    }

    /// Returns true if the buffer is persistently mapped in memory.
    pub fn uses_persistent_mapping(&self) -> bool {
        self.persistent_mapping.is_some()
    }

    /// Changes the type of the buffer. Returns `Err` if this is forbidden.
    pub fn set_type(mut self, ty: BufferType) -> Result<Buffer, Buffer> {
        // FIXME: return Err for GLES2
        self.ty = ty;
        Ok(self)
    }

    /// Uploads data in the buffer.
    ///
    /// The data must fit inside the buffer.
    ///
    /// # Panic
    ///
    /// Panics if `offset_bytes` is out of range or the data is too large to fit in the buffer.
    ///
    /// # Unsafety
    ///
    /// If the buffer uses persistent mapping, the caller of this function must handle
    /// synchronization.
    ///
    pub unsafe fn upload<D>(&self, offset_bytes: usize, data: &[D])
                            where D: Copy + Send + 'static
    {
        let to_upload = mem::size_of::<D>() * data.len();
        assert!(offset_bytes + to_upload <= self.size);

        if self.persistent_mapping.is_some() {
            let mut mapping = self.map(offset_bytes, data.len());
            ptr::copy_nonoverlapping(data.as_ptr(), mapping.deref_mut().as_mut_ptr(), data.len());

        } else if self.immutable {
            let mut ctxt = self.context.make_current();

            let (tmp_buffer, _, _) = create_buffer(&mut ctxt, to_upload, Some(data),
                                                   BufferType::CopyReadBuffer,
                                                   true, true).unwrap();
            copy_buffer(&mut ctxt, tmp_buffer, 0, self.id, offset_bytes, to_upload);
            destroy_buffer(&mut ctxt, tmp_buffer);

        } else {
            assert!(offset_bytes < self.size);

            let invalidate_all = offset_bytes == 0 && to_upload == self.size;

            let mut ctxt = self.context.make_current();

            if invalidate_all && (ctxt.version >= &Version(Api::Gl, 4, 3) ||
                ctxt.extensions.gl_arb_invalidate_subdata)
            {
                ctxt.gl.InvalidateBufferData(self.id);
            }

            if ctxt.version >= &Version(Api::Gl, 4, 5) {
                ctxt.gl.NamedBufferSubData(self.id, offset_bytes as gl::types::GLintptr,
                                           to_upload as gl::types::GLsizei,
                                           data.as_ptr() as *const libc::c_void)

            } else if ctxt.extensions.gl_ext_direct_state_access {
                ctxt.gl.NamedBufferSubDataEXT(self.id, offset_bytes as gl::types::GLintptr,
                                              to_upload as gl::types::GLsizeiptr,
                                              data.as_ptr() as *const libc::c_void)

            } else if ctxt.version >= &Version(Api::Gl, 1, 5) ||
                ctxt.version >= &Version(Api::GlEs, 2, 0)
            {
                let bind = bind_buffer(&mut ctxt, self.id, self.ty);
                ctxt.gl.BufferSubData(bind, offset_bytes as gl::types::GLintptr,
                                      to_upload as gl::types::GLsizeiptr,
                                      data.as_ptr() as *const libc::c_void);

            } else if ctxt.extensions.gl_arb_vertex_buffer_object {
                let bind = bind_buffer(&mut ctxt, self.id, self.ty);
                ctxt.gl.BufferSubDataARB(bind, offset_bytes as gl::types::GLintptr,
                                         to_upload as gl::types::GLsizeiptr,
                                         data.as_ptr() as *const libc::c_void);

            } else {
                unreachable!();
            }
        }
    }

    /// Returns a mapping in memory of the content of the buffer.
    ///
    /// There are two possibilities:
    ///
    ///  - If the buffer uses persistent mapping, it will simply return a wrapper around the
    ///    pointer to the existing mapping.
    ///  - If the buffer doesn't use persistent mapping, it will create a temporary buffer which
    ///    will be mapped. After the mapping is released, the temporary buffer will be copied
    ///    to the real buffer.
    ///
    /// In the second case, the changes will only be written when the mapping is released.
    /// Therefore this API is error-prone and shouldn't be exposed directly to the user. Instead
    /// `map` public functions should take a `&mut self` instead of a `&self` to prevent users
    /// from manipulating the buffer while it is "mapped".
    ///
    /// # Unsafety
    ///
    /// If the buffer uses persistent mapping, the caller of this function must handle
    /// synchronization.
    ///
    pub unsafe fn map<D>(&self, offset_bytes: usize, elements: usize)
                         -> Mapping<D> where D: Copy + Send + 'static
    {
        assert!(offset_bytes % mem::size_of::<D>() == 0);
        assert!(offset_bytes <= self.size);
        assert!(offset_bytes + elements * mem::size_of::<D>() <= self.size);

        if let Some(existing_mapping) = self.persistent_mapping.clone() {
            Mapping {
                buffer: self,
                temporary_buffer: None,
                data: (existing_mapping as *mut u8).offset(offset_bytes as isize) as *mut D,
                len: elements,
            }

        } else {
            let size_bytes = elements * mem::size_of::<D>();

            // we have to construct a temporary buffer that we will map in memory
            // then after the Mapping is destroyed, we will copy from the temporary buffer to the
            // real one
            let temporary_buffer = {
                let mut ctxt = self.context.make_current();
                let (temporary_buffer, _, _) = create_buffer::<D>(&mut ctxt, size_bytes,
                                                                  None, BufferType::CopyWriteBuffer,
                                                                  true, true).unwrap();
                temporary_buffer
            };

            let ptr = {
                let mut ctxt = self.context.make_current();

                copy_buffer(&mut ctxt, self.id, offset_bytes, temporary_buffer, 0, size_bytes);

                if ctxt.version >= &Version(Api::Gl, 4, 5) {
                    ctxt.gl.MapNamedBufferRange(temporary_buffer, 0, size_bytes as gl::types::GLsizei,
                                                gl::MAP_READ_BIT | gl::MAP_WRITE_BIT |
                                                gl::MAP_FLUSH_EXPLICIT_BIT)

                } else if ctxt.version >= &Version(Api::Gl, 3, 0) ||
                    ctxt.version >= &Version(Api::GlEs, 3, 0) ||
                    ctxt.extensions.gl_arb_map_buffer_range
                {
                    let bind = bind_buffer(&mut ctxt, temporary_buffer, self.ty);
                    ctxt.gl.MapBufferRange(bind, 0, size_bytes as gl::types::GLsizeiptr,
                                           gl::MAP_READ_BIT | gl::MAP_WRITE_BIT |
                                           gl::MAP_FLUSH_EXPLICIT_BIT)

                } else {
                    unimplemented!();       // FIXME: 
                }
            };

            Mapping {
                buffer: self,
                temporary_buffer: Some((temporary_buffer, offset_bytes)),
                data: ptr as *mut D,
                len: elements,
            }

        }
    }

    /// Reads the content of the buffer. Returns `None` if this operation is not supported.
    ///
    /// # Panic
    ///
    /// Panicks if out of range.
    ///
    /// # Unsafety
    ///
    /// If the buffer uses persistent mapping, the caller of this function must handle
    /// synchronization.
    ///
    pub unsafe fn read_if_supported<D>(&self, offset_bytes: usize, output: &mut [D])
                                       -> Result<(), ()> where D: Copy + Send + 'static
    {
        assert!(offset_bytes <= self.size);
        assert!(offset_bytes + output.len() * mem::size_of::<D>() <= self.size);

        if self.persistent_mapping.is_some() {
            let mapping = self.map(offset_bytes, output.len());
            ptr::copy_nonoverlapping(mapping.as_ptr(), output.as_mut_ptr(),
                                     output.len() * mem::size_of::<D>());
            Ok(())

        } else {
            let mut ctxt = self.context.make_current();

            if ctxt.version >= &Version(Api::Gl, 4, 5) {
                ctxt.gl.GetNamedBufferSubData(self.id, offset_bytes as gl::types::GLintptr,
                                              (output.len() * mem::size_of::<D>())
                                              as gl::types::GLsizei, output.as_mut_ptr()
                                              as *mut libc::c_void);

            } else if ctxt.version >= &Version(Api::Gl, 1, 5) {
                let bind = bind_buffer(&mut ctxt, self.id, self.ty);
                ctxt.gl.GetBufferSubData(bind, offset_bytes as gl::types::GLintptr,
                                         (output.len() * mem::size_of::<D>())
                                         as gl::types::GLsizeiptr, output.as_mut_ptr()
                                         as *mut libc::c_void);

            } else if ctxt.extensions.gl_arb_vertex_buffer_object {
                let bind = bind_buffer(&mut ctxt, self.id, self.ty);
                ctxt.gl.GetBufferSubDataARB(bind, offset_bytes as gl::types::GLintptr,
                                            (output.len() * mem::size_of::<D>())
                                            as gl::types::GLsizeiptr, output.as_mut_ptr()
                                            as *mut libc::c_void);

            } else if ctxt.version >= &Version(Api::GlEs, 1, 0) {
                return Err(());

            } else {
                unreachable!()
            }

            Ok(())
        }
    }
}

impl fmt::Debug for Buffer {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        write!(fmt, "Buffer #{} (size: {} bytes)", self.id, self.size)
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        unsafe {
            let mut ctxt = self.context.make_current();
            self.context.vertex_array_objects.purge_buffer(&mut ctxt, self.id);
            destroy_buffer(&mut ctxt, self.id);
        }
    }
}

impl GlObject for Buffer {
    type Id = gl::types::GLuint;
    fn get_id(&self) -> gl::types::GLuint {
        self.id
    }
}

unsafe impl<'a, D> Sync for Mapping<'a, D> where D: Sync {}

impl<'a, D> Drop for Mapping<'a, D> {
    fn drop(&mut self) {
        let mut ctxt = self.buffer.context.make_current();

        // flushing the written data
        let to_flush = if let Some((temporary_buffer, _)) = self.temporary_buffer {
            temporary_buffer
        } else {
            self.buffer.id
        };

        if ctxt.version >= &Version(Api::Gl, 4, 5) || ctxt.extensions.gl_arb_direct_state_access {
            unsafe {
                ctxt.gl.FlushMappedNamedBufferRange(to_flush, 0,
                                                    (self.len * mem::size_of::<D>())
                                                    as gl::types::GLsizei);
            }

        } else if ctxt.version >= &Version(Api::Gl, 3, 0) ||
                  ctxt.version >= &Version(Api::GlEs, 3, 0) ||
                  ctxt.extensions.gl_arb_map_buffer_range
        {
            unsafe {
                let bind = bind_buffer(&mut ctxt, to_flush, self.buffer.ty);
                ctxt.gl.FlushMappedBufferRange(bind, 0, (self.len * mem::size_of::<D>())
                                               as gl::types::GLsizeiptr)
            }

        } else {
            unreachable!();
        }

        // don't unmap if the buffer is persistent
        if self.buffer.uses_persistent_mapping() {
            return;
        }

        if let Some((temporary_buffer, offset_bytes)) = self.temporary_buffer {
            unsafe {
                if ctxt.version >= &Version(Api::Gl, 4, 5) {
                    ctxt.gl.UnmapNamedBuffer(temporary_buffer);

                } else if ctxt.version >= &Version(Api::Gl, 1, 5) ||
                    ctxt.version >= &Version(Api::GlEs, 3, 0)
                {
                    let bind = bind_buffer(&mut ctxt, temporary_buffer, self.buffer.ty);
                    ctxt.gl.UnmapBuffer(bind);

                } else if ctxt.extensions.gl_arb_vertex_buffer_object {
                    let bind = bind_buffer(&mut ctxt, temporary_buffer, self.buffer.ty);
                    ctxt.gl.UnmapBufferARB(bind);

                } else {
                    unreachable!();
                }

                copy_buffer(&mut ctxt, temporary_buffer, 0, self.buffer.id, offset_bytes,
                            self.len * mem::size_of::<D>());

                destroy_buffer(&mut ctxt, temporary_buffer);
            }

        } else {
            unsafe {
                if ctxt.version >= &Version(Api::Gl, 4, 5) {
                    ctxt.gl.UnmapNamedBuffer(self.buffer.id);

                } else if ctxt.version >= &Version(Api::Gl, 1, 5) ||
                    ctxt.version >= &Version(Api::GlEs, 3, 0)
                {
                    let bind = bind_buffer(&mut ctxt, self.buffer.id, self.buffer.ty);
                    ctxt.gl.UnmapBuffer(bind);

                } else if ctxt.extensions.gl_arb_vertex_buffer_object {
                    let bind = bind_buffer(&mut ctxt, self.buffer.id, self.buffer.ty);
                    ctxt.gl.UnmapBufferARB(bind);

                } else {
                    unreachable!();
                }
            }
        }
    }
}

impl<'a, D> Deref for Mapping<'a, D> {
    type Target = [D];
    fn deref<'b>(&'b self) -> &'b [D] {
        unsafe {
            slice::from_raw_parts_mut(self.data, self.len)
        }
    }
}

impl<'a, D> DerefMut for Mapping<'a, D> {
    fn deref_mut<'b>(&'b mut self) -> &'b mut [D] {
        unsafe {
            slice::from_raw_parts_mut(self.data, self.len)
        }
    }
}

/// Creates a new buffer.
///
/// # Panic
///
/// Panics if `data.len() * size_of::<D>() < size` or if `size % size_of::<D>() != 0`.
unsafe fn create_buffer<D>(mut ctxt: &mut CommandContext, size: usize, data: Option<&[D]>,
                           ty: BufferType, dynamic: bool, avoid_persistent: bool)
                           -> Result<(gl::types::GLuint, bool, Option<*mut libc::c_void>),
                                     BufferCreationError>
                           where D: Send + Copy + 'static
{
    if let Some(ref data) = data {
        assert!(data.len() * mem::size_of::<D>() >= size);
        assert!(size % mem::size_of::<D>() == 0);
    }

    let mut id: gl::types::GLuint = mem::uninitialized();

    if ctxt.version >= &Version(Api::Gl, 4, 5) || ctxt.extensions.gl_arb_direct_state_access {
        ctxt.gl.CreateBuffers(1, &mut id);
    } else if ctxt.version >= &Version(Api::Gl, 1, 5) ||
        ctxt.version >= &Version(Api::GlEs, 2, 0)
    {
        ctxt.gl.GenBuffers(1, &mut id);
    } else if ctxt.extensions.gl_arb_vertex_buffer_object {
        ctxt.gl.GenBuffersARB(1, &mut id);
    } else {
        unreachable!();
    }

    let data_ptr = if let Some(data) = data {
        if size == 0 {
            ptr::null()
        } else {
            data.as_ptr()
        }
    } else {
        ptr::null()
    };

    let size = match size {
        0 => 1,     // use size 1 instead of 0, or nvidia drivers complain
        a => a
    };

    let mut obtained_size: gl::types::GLint = mem::uninitialized();
    let immutable: bool;

    let mutable_storage_flags = if dynamic {
        gl::DYNAMIC_DRAW
    } else {
        gl::STATIC_DRAW
    };

    let immutable_storage_flags = if dynamic && avoid_persistent {
        gl::DYNAMIC_STORAGE_BIT | gl::MAP_READ_BIT | gl::MAP_WRITE_BIT
    } else if dynamic {
        gl::MAP_PERSISTENT_BIT | gl::MAP_READ_BIT | gl::MAP_WRITE_BIT
    } else {
        0
    };

    if ctxt.version >= &Version(Api::Gl, 4, 5) || ctxt.extensions.gl_arb_direct_state_access {
        ctxt.gl.NamedBufferStorage(id, size as gl::types::GLsizei,
                                   data_ptr as *const libc::c_void,
                                   immutable_storage_flags);
        ctxt.gl.GetNamedBufferParameteriv(id, gl::BUFFER_SIZE, &mut obtained_size);
        immutable = !avoid_persistent;

    } else if ctxt.extensions.gl_arb_buffer_storage &&
              ctxt.extensions.gl_ext_direct_state_access
    {
        ctxt.gl.NamedBufferStorageEXT(id, size as gl::types::GLsizeiptr,
                                      data_ptr as *const libc::c_void,
                                      immutable_storage_flags);
        ctxt.gl.GetNamedBufferParameterivEXT(id, gl::BUFFER_SIZE, &mut obtained_size);
        immutable = !avoid_persistent;

    } else if ctxt.version >= &Version(Api::Gl, 4, 4) ||
              ctxt.extensions.gl_arb_buffer_storage
    {
        let bind = bind_buffer(&mut ctxt, id, ty);
        ctxt.gl.BufferStorage(bind, size as gl::types::GLsizeiptr,
                              data_ptr as *const libc::c_void,
                              immutable_storage_flags);
        ctxt.gl.GetBufferParameteriv(bind, gl::BUFFER_SIZE, &mut obtained_size);
        immutable = !avoid_persistent;

    } else if ctxt.version >= &Version(Api::Gl, 1, 5) ||
        ctxt.version >= &Version(Api::GlEs, 2, 0)
    {
        let bind = bind_buffer(&mut ctxt, id, ty);
        ctxt.gl.BufferData(bind, size as gl::types::GLsizeiptr,
                           data_ptr as *const libc::c_void, mutable_storage_flags);
        ctxt.gl.GetBufferParameteriv(bind, gl::BUFFER_SIZE, &mut obtained_size);
        immutable = false;

    } else if ctxt.extensions.gl_arb_vertex_buffer_object {
        let bind = bind_buffer(&mut ctxt, id, ty);
        ctxt.gl.BufferDataARB(bind, size as gl::types::GLsizeiptr,
                              data_ptr as *const libc::c_void, mutable_storage_flags);
        ctxt.gl.GetBufferParameterivARB(bind, gl::BUFFER_SIZE, &mut obtained_size);
        immutable = false;

    } else {
        unreachable!();
    }

    if size != obtained_size as usize {
        if ctxt.version >= &Version(Api::Gl, 1, 5) ||
            ctxt.version >= &Version(Api::GlEs, 2, 0)
        {
            ctxt.gl.DeleteBuffers(1, [id].as_ptr());
        } else if ctxt.extensions.gl_arb_vertex_buffer_object {
            ctxt.gl.DeleteBuffersARB(1, [id].as_ptr());
        } else {
            unreachable!();
        }
        
        return Err(BufferCreationError::OutOfMemory);
    }

    let persistent_mapping = if immutable && dynamic && !avoid_persistent {
        let ptr = if ctxt.version >= &Version(Api::Gl, 4, 5) {
            ctxt.gl.MapNamedBufferRange(id, 0, size as gl::types::GLsizei,
                                        gl::MAP_READ_BIT | gl::MAP_WRITE_BIT |
                                        gl::MAP_PERSISTENT_BIT | gl::MAP_FLUSH_EXPLICIT_BIT)

        } else if ctxt.version >= &Version(Api::Gl, 3, 0) ||
                  ctxt.extensions.gl_arb_map_buffer_range
        {
            let bind = bind_buffer(&mut ctxt, id, ty);
            ctxt.gl.MapBufferRange(bind, 0, size as gl::types::GLsizeiptr,
                                   gl::MAP_READ_BIT | gl::MAP_WRITE_BIT |
                                   gl::MAP_PERSISTENT_BIT | gl::MAP_FLUSH_EXPLICIT_BIT)
        } else {
            unreachable!();
        };

        if ptr.is_null() {
            let error = ::get_gl_error(ctxt);
            panic!("glMapBufferRange returned null (error: {:?})", error);
        }

        Some(ptr)

    } else {
        None
    };

    Ok((id, immutable, persistent_mapping))
}

/// Binds a buffer of the given type, and returns the GLenum of the bind point.
unsafe fn bind_buffer(mut ctxt: &mut CommandContext, id: gl::types::GLuint, ty: BufferType)
                      -> gl::types::GLenum
{
    match ty {
        BufferType::ArrayBuffer => {
            if ctxt.state.array_buffer_binding != id {
                ctxt.state.array_buffer_binding = id;

                if ctxt.version >= &Version(Api::Gl, 1, 5) ||
                    ctxt.version >= &Version(Api::GlEs, 2, 0)
                {
                    ctxt.gl.BindBuffer(gl::ARRAY_BUFFER, id);
                } else if ctxt.extensions.gl_arb_vertex_buffer_object {
                    ctxt.gl.BindBufferARB(gl::ARRAY_BUFFER, id);    // bind points are the same in the ext
                } else {
                    unreachable!();
                }
            }

            gl::ARRAY_BUFFER
        },

        BufferType::PixelPackBuffer => {
            if ctxt.state.pixel_pack_buffer_binding != id {
                ctxt.state.pixel_pack_buffer_binding = id;

                if ctxt.version >= &Version(Api::Gl, 1, 5) ||
                    ctxt.version >= &Version(Api::GlEs, 2, 0)
                {
                    ctxt.gl.BindBuffer(gl::PIXEL_PACK_BUFFER, id);
                } else if ctxt.extensions.gl_arb_vertex_buffer_object {
                    ctxt.gl.BindBufferARB(gl::PIXEL_PACK_BUFFER, id);    // bind points are the same in the ext
                } else {
                    unreachable!();
                }
            }

            gl::PIXEL_PACK_BUFFER
        },

        BufferType::PixelUnpackBuffer => {
            if ctxt.state.pixel_unpack_buffer_binding != id {
                ctxt.state.pixel_unpack_buffer_binding = id;

                if ctxt.version >= &Version(Api::Gl, 1, 5) ||
                    ctxt.version >= &Version(Api::GlEs, 2, 0)
                {
                    ctxt.gl.BindBuffer(gl::PIXEL_UNPACK_BUFFER, id);
                } else if ctxt.extensions.gl_arb_vertex_buffer_object {
                    ctxt.gl.BindBufferARB(gl::PIXEL_UNPACK_BUFFER, id);    // bind points are the same in the ext
                } else {
                    unreachable!();
                }
            }

            gl::PIXEL_UNPACK_BUFFER
        },

        BufferType::UniformBuffer => {
            if ctxt.state.uniform_buffer_binding != id {
                ctxt.state.uniform_buffer_binding = id;

                if ctxt.version >= &Version(Api::Gl, 1, 5) ||
                    ctxt.version >= &Version(Api::GlEs, 2, 0)
                {
                    ctxt.gl.BindBuffer(gl::UNIFORM_BUFFER, id);
                } else if ctxt.extensions.gl_arb_vertex_buffer_object {
                    ctxt.gl.BindBufferARB(gl::UNIFORM_BUFFER, id);    // bind points are the same in the ext
                } else {
                    unreachable!();
                }
            }

            gl::UNIFORM_BUFFER
        },

        BufferType::CopyReadBuffer => {
            if ctxt.state.copy_read_buffer_binding != id {
                ctxt.state.copy_read_buffer_binding = id;

                if ctxt.version >= &Version(Api::Gl, 1, 5) ||
                    ctxt.version >= &Version(Api::GlEs, 2, 0)
                {
                    ctxt.gl.BindBuffer(gl::COPY_READ_BUFFER, id);
                } else if ctxt.extensions.gl_arb_vertex_buffer_object {
                    ctxt.gl.BindBufferARB(gl::COPY_READ_BUFFER, id);    // bind points are the same in the ext
                } else {
                    unreachable!();
                }
            }

            gl::COPY_READ_BUFFER
        },

        BufferType::CopyWriteBuffer => {
            if ctxt.state.copy_write_buffer_binding != id {
                ctxt.state.copy_write_buffer_binding = id;

                if ctxt.version >= &Version(Api::Gl, 1, 5) ||
                    ctxt.version >= &Version(Api::GlEs, 2, 0)
                {
                    ctxt.gl.BindBuffer(gl::COPY_WRITE_BUFFER, id);
                } else if ctxt.extensions.gl_arb_vertex_buffer_object {
                    ctxt.gl.BindBufferARB(gl::COPY_WRITE_BUFFER, id);    // bind points are the same in the ext
                } else {
                    unreachable!();
                }
            }

            gl::COPY_WRITE_BUFFER
        },
    }
}

/// Copies from a buffer to another.
unsafe fn copy_buffer(ctxt: &mut CommandContext, source: gl::types::GLuint,
                      source_offset: usize, dest: gl::types::GLuint, dest_offset: usize,
                      size: usize)
{
    if ctxt.version >= &Version(Api::Gl, 4, 5) || ctxt.extensions.gl_arb_direct_state_access {
        ctxt.gl.CopyNamedBufferSubData(source, dest, source_offset as gl::types::GLintptr,
                                       dest_offset as gl::types::GLintptr,
                                       size as gl::types::GLsizei);

    } else if ctxt.extensions.gl_ext_direct_state_access {
        ctxt.gl.NamedCopyBufferSubDataEXT(source, dest, source_offset as gl::types::GLintptr,
                                          dest_offset as gl::types::GLintptr,
                                          size as gl::types::GLsizeiptr);

    } else if ctxt.version >= &Version(Api::Gl, 3, 1) || ctxt.version >= &Version(Api::GlEs, 3, 0)
           || ctxt.extensions.gl_arb_copy_buffer || ctxt.extensions.gl_nv_copy_buffer
    {
        fn find_bind_point(ctxt: &mut CommandContext, id: gl::types::GLuint)
                           -> Option<gl::types::GLenum>
        {
            if ctxt.state.array_buffer_binding == id {
                Some(gl::ARRAY_BUFFER)
            } else if ctxt.state.pixel_pack_buffer_binding == id {
                Some(gl::PIXEL_PACK_BUFFER)
            } else if ctxt.state.pixel_unpack_buffer_binding == id {
                Some(gl::PIXEL_UNPACK_BUFFER)
            } else if ctxt.state.uniform_buffer_binding == id {
                Some(gl::UNIFORM_BUFFER)
            } else if ctxt.state.copy_read_buffer_binding == id {
                Some(gl::COPY_READ_BUFFER)
            } else if ctxt.state.copy_write_buffer_binding == id {
                Some(gl::COPY_WRITE_BUFFER)
            } else {
                None
            }
        }

        let source_bind_point = match find_bind_point(ctxt, source) {
            Some(p) => p,
            None => {
                // if the source is not binded and the destination is binded to COPY_READ,
                // we bind the source to COPY_WRITE instead, to avoid a state change
                if ctxt.state.copy_read_buffer_binding == dest {
                    bind_buffer(ctxt, source, BufferType::CopyWriteBuffer)
                } else {
                    bind_buffer(ctxt, source, BufferType::CopyReadBuffer)
                }
            }
        };

        let dest_bind_point = match find_bind_point(ctxt, dest) {
            Some(p) => p,
            None => bind_buffer(ctxt, dest, BufferType::CopyWriteBuffer)
        };

        if ctxt.version >= &Version(Api::Gl, 3, 1) || ctxt.version >= &Version(Api::GlEs, 3, 0)
            || ctxt.extensions.gl_arb_copy_buffer
        {
            ctxt.gl.CopyBufferSubData(source_bind_point, dest_bind_point,
                                      source_offset as gl::types::GLintptr,
                                      dest_offset as gl::types::GLintptr,
                                      size as gl::types::GLsizeiptr);
        } else if ctxt.extensions.gl_nv_copy_buffer {
            ctxt.gl.CopyBufferSubDataNV(source_bind_point, dest_bind_point,
                                        source_offset as gl::types::GLintptr,
                                        dest_offset as gl::types::GLintptr,
                                        size as gl::types::GLsizeiptr);
        } else {
            unreachable!();
        }

    } else {
        // TODO: use proper error result
        panic!("Buffers copy are not supported");
    }
}

/// Destroys a buffer.
unsafe fn destroy_buffer(mut ctxt: &mut CommandContext, id: gl::types::GLuint) {
    // FIXME: uncomment this and move it from Buffer's destructor
    //self.context.vertex_array_objects.purge_buffer(&mut ctxt, id);

    if ctxt.state.array_buffer_binding == id {
        ctxt.state.array_buffer_binding = 0;
    }

    if ctxt.state.pixel_pack_buffer_binding == id {
        ctxt.state.pixel_pack_buffer_binding = 0;
    }

    if ctxt.state.pixel_unpack_buffer_binding == id {
        ctxt.state.pixel_unpack_buffer_binding = 0;
    }

    if ctxt.state.uniform_buffer_binding == id {
        ctxt.state.uniform_buffer_binding = 0;
    }

    if ctxt.version >= &Version(Api::Gl, 1, 5) ||
        ctxt.version >= &Version(Api::GlEs, 2, 0)
    {
        ctxt.gl.DeleteBuffers(1, [id].as_ptr());
    } else if ctxt.extensions.gl_arb_vertex_buffer_object {
        ctxt.gl.DeleteBuffersARB(1, [id].as_ptr());
    } else {
        unreachable!();
    }
}
