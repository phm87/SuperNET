# Copyright (c) 2013 The Chromium Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# GNU Makefile based on shared rules provided by the Native Client SDK.
# See README.Makefiles for more details.

VALID_TOOLCHAINS := pnacl newlib glibc clang-newlib mac 

NACL_SDK_ROOT ?= $(abspath $(CURDIR))

TARGET = iguana

#EXTRA= -D__PNACL -D__PNACL__ -DNN_DISABLE_GETADDRINFO_A=1 -DNN_USE_LITERAL_IFADDR=1 -DNN_HAVE_STDINT=1 -DNN_HAVE_MSG_CONTROL=0 -DNN_HAVE_SEMAPHORE=1 -DNN_HAVE_POLL=1 -DNN_HAVE_SOCKETPAIR=1 -DNN_USE_POLL=1
EXTRA= -D__PNACL -D__PNACL__ -DHAVE_CONFIG_H

#include $(NACL_SDK_ROOT)/tools/common.mk
include tools/common.mk
include iguana.sources
include ../crypto777/crypto777.sources

CHROME_ARGS += --allow-nacl-socket-api=127.0.0.1

DEPS = nacl_io
#LIBS = curl  ssl crypto z glibc-compat nacl_spawn ppapi nacl_io ppapi_cpp  ppapi_simple #  cli_main
#LIBS = crypto777  curl ssl crypto z pthread ppapi nacl_io
#DEPS = nacl_io
#LIBS = crypto777 curl  ssl crypto z glibc-compat nacl_spawn ppapi nacl_io ppapi_simple  #  cli_main ppapi_cpp ppapi_simple
LIBS = crypto777 curl  ssl crypto z glibc-compat nacl_spawn ppapi nacl_io ppapi_simple 

CFLAGS = -Wall -O2  -DLIQUIDITY_PROVIDER=1 -fno-strict-aliasing $(EXTRA)
LFLAGS = libs

# Build rules generated by macros from common.mk:

$(foreach dep,$(DEPS),$(eval $(call DEPEND_RULE,$(dep))))

$(foreach src,$(SOURCES),$(eval $(call COMPILE_RULE,$(src),$(CFLAGS))))

# The PNaCl workflow uses both an unstripped and finalized/stripped binary.
# On NaCl, only produce a stripped binary for Release configs (not Debug).
ifneq (,$(or $(findstring pnacl,$(TOOLCHAIN)),$(findstring Release,$(CONFIG))))

$(eval $(call LINK_RULE,$(TARGET)_unstripped,$(SOURCES) $(LOCALLIBS),$(LIBS),$(DEPS)));
$(eval $(call STRIP_RULE,$(TARGET),$(TARGET)_unstripped))

else
$(eval $(call LINK_RULE,$(TARGET),$(SOURCES),$(LIBS),$(DEPS)))

endif

$(eval $(call NMF_RULE,$(TARGET),))

